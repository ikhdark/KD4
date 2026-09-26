//! Cloud config bundle load orchestration.
//!
//! Each loader fetches one bundle from the authenticated backend with bounded
//! retries and validates it before it can become configuration. The result is
//! held only in memory by `CloudConfigBundleLoader`: a client-written copy could
//! not prove backend origin, so nothing is persisted or refreshed behind the
//! already-snapshotted runtime config.

use crate::backend::BundleClient;
use crate::backend::BundleRequestError;
use crate::backend::RetryableFailureKind;
use crate::metrics::emit_fetch_attempt_metric;
use crate::metrics::emit_fetch_final_metric;
use crate::metrics::emit_load_metric;
use crate::validation::validate_bundle;
use codex_config::AbsolutePathBuf;
use codex_config::CloudConfigBundle;
use codex_config::CloudConfigBundleLoadError;
use codex_config::CloudConfigBundleLoadErrorCode;
use codex_core::retry::backoff;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::RefreshTokenError;
use codex_login::UnauthorizedRecovery;
use codex_protocol::account::PlanType;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;

pub(crate) const CLOUD_CONFIG_BUNDLE_TIMEOUT: Duration = Duration::from_secs(15);
const CLOUD_CONFIG_BUNDLE_MAX_ATTEMPTS: usize = 5;
const CLOUD_CONFIG_BUNDLE_LOAD_FAILED_MESSAGE: &str =
    "Failed to load cloud config bundle (workspace-managed policies).";
const CLOUD_CONFIG_BUNDLE_AUTH_RECOVERY_FAILED_MESSAGE: &str = concat!(
    "Your authentication session could not be refreshed automatically. ",
    "Please log out and sign in again."
);

fn cloud_config_eligible_auth(auth: &CodexAuth) -> bool {
    let Some(plan_type) = auth.account_plan_type() else {
        return false;
    };
    auth.uses_codex_backend()
        && (plan_type.is_business_like()
            || matches!(plan_type, PlanType::Enterprise | PlanType::Edu))
}

fn optional_bundle(bundle: CloudConfigBundle) -> Option<CloudConfigBundle> {
    if bundle.is_empty() {
        None
    } else {
        Some(bundle)
    }
}

enum UnauthorizedRecoveryAction {
    RetrySameAttempt,
    RetryNextAttempt,
}

pub(crate) struct CloudConfigBundleService<C> {
    auth_manager: Arc<AuthManager>,
    client: Arc<C>,
    codex_home: AbsolutePathBuf,
    timeout: Duration,
}

impl<C> Clone for CloudConfigBundleService<C> {
    fn clone(&self) -> Self {
        Self {
            auth_manager: Arc::clone(&self.auth_manager),
            client: Arc::clone(&self.client),
            codex_home: self.codex_home.clone(),
            timeout: self.timeout,
        }
    }
}

impl<C> CloudConfigBundleService<C>
where
    C: BundleClient + 'static,
{
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        client: Arc<C>,
        codex_home: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self {
            auth_manager,
            client,
            codex_home: AbsolutePathBuf::resolve_path_against_base(codex_home, "/"),
            timeout,
        }
    }

    pub(crate) async fn load_startup_bundle_with_timeout(
        &self,
    ) -> Result<Option<CloudConfigBundle>, CloudConfigBundleLoadError> {
        let _timer =
            codex_otel::start_global_timer("codex.cloud_config_bundle.fetch.duration_ms", &[]);
        let started_at = Instant::now();
        let load_result = self.load_startup_bundle().await;

        let result = match load_result {
            Ok(result) => result,
            Err(err) => {
                emit_load_metric("startup", "error", /*bundle*/ None);
                return Err(err);
            }
        };

        match result.as_ref() {
            Some(bundle) => {
                tracing::info!(
                    elapsed_ms = started_at.elapsed().as_millis(),
                    config_fragments = bundle.config_toml.enterprise_managed.len(),
                    requirements_fragments = bundle.requirements_toml.enterprise_managed.len(),
                    "Cloud config bundle load completed"
                );
                emit_load_metric("startup", "success", Some(bundle));
            }
            None => {
                tracing::info!(
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "Cloud config bundle load completed (none)"
                );
                emit_load_metric("startup", "success", /*bundle*/ None);
            }
        }

        Ok(result)
    }

    async fn load_startup_bundle(
        &self,
    ) -> Result<Option<CloudConfigBundle>, CloudConfigBundleLoadError> {
        let bundle = timeout(self.timeout, async {
            let Some(auth) = self.auth_manager.auth().await else {
                return Ok(None);
            };
            if !cloud_config_eligible_auth(&auth) {
                return Ok(None);
            }
            self.fetch_remote_bundle_with_retries(auth, "startup")
                .await
                .map(Some)
        })
        .await
        .map_err(|_| {
            CloudConfigBundleLoadError::new(
                CloudConfigBundleLoadErrorCode::Timeout,
                None,
                format!(
                    "timed out waiting for cloud config bundle after {}s",
                    self.timeout.as_secs()
                ),
            )
        })??;
        Ok(bundle.and_then(optional_bundle))
    }

    async fn fetch_remote_bundle_with_retries(
        &self,
        mut auth: CodexAuth,
        trigger: &'static str,
    ) -> Result<CloudConfigBundle, CloudConfigBundleLoadError> {
        let mut attempt = 1;
        let mut last_status_code: Option<u16> = None;
        let mut auth_recovery = self.auth_manager.unauthorized_recovery();

        while attempt <= CLOUD_CONFIG_BUNDLE_MAX_ATTEMPTS {
            match self.client.get_bundle(&auth).await {
                Ok(bundle) => {
                    self.validate_remote_bundle(trigger, attempt, &bundle)?;
                    return Ok(bundle);
                }
                Err(BundleRequestError::Permanent { status_code }) => {
                    emit_fetch_attempt_metric(trigger, attempt, "error", status_code);
                    emit_fetch_final_metric(
                        trigger,
                        "error",
                        "request_permanent",
                        attempt,
                        status_code,
                        None,
                    );
                    return Err(CloudConfigBundleLoadError::new(
                        CloudConfigBundleLoadErrorCode::RequestFailed,
                        status_code,
                        CLOUD_CONFIG_BUNDLE_LOAD_FAILED_MESSAGE,
                    ));
                }
                Err(BundleRequestError::Retryable(status)) => {
                    last_status_code = status.status_code();
                    if self
                        .retry_after_request_failure(trigger, attempt, status)
                        .await
                    {
                        attempt += 1;
                        continue;
                    }
                }
                Err(BundleRequestError::Unauthorized {
                    status_code,
                    message,
                }) => {
                    last_status_code = status_code;
                    match self
                        .handle_unauthorized(
                            &mut auth,
                            &mut auth_recovery,
                            trigger,
                            attempt,
                            status_code,
                            &message,
                        )
                        .await?
                    {
                        UnauthorizedRecoveryAction::RetrySameAttempt => continue,
                        UnauthorizedRecoveryAction::RetryNextAttempt => {
                            attempt += 1;
                            continue;
                        }
                    }
                }
            }

            break;
        }

        emit_fetch_final_metric(
            trigger,
            "error",
            "request_retry_exhausted",
            CLOUD_CONFIG_BUNDLE_MAX_ATTEMPTS,
            last_status_code,
            /*bundle*/ None,
        );
        tracing::error!("{CLOUD_CONFIG_BUNDLE_LOAD_FAILED_MESSAGE}");
        Err(CloudConfigBundleLoadError::new(
            CloudConfigBundleLoadErrorCode::RequestFailed,
            last_status_code,
            CLOUD_CONFIG_BUNDLE_LOAD_FAILED_MESSAGE,
        ))
    }

    fn validate_remote_bundle(
        &self,
        trigger: &'static str,
        attempt: usize,
        bundle: &CloudConfigBundle,
    ) -> Result<(), CloudConfigBundleLoadError> {
        emit_fetch_attempt_metric(trigger, attempt, "success", /*status_code*/ None);
        if let Err(err) = validate_bundle(bundle, &self.codex_home) {
            emit_fetch_final_metric(
                trigger,
                "error",
                "invalid_bundle",
                attempt,
                /*status_code*/ None,
                /*bundle*/ None,
            );
            return Err(err);
        }

        emit_fetch_final_metric(
            trigger,
            "success",
            "none",
            attempt,
            /*status_code*/ None,
            Some(bundle),
        );
        Ok(())
    }

    async fn retry_after_request_failure(
        &self,
        trigger: &'static str,
        attempt: usize,
        status: RetryableFailureKind,
    ) -> bool {
        let status_code = status.status_code();
        emit_fetch_attempt_metric(trigger, attempt, "error", status_code);
        if attempt < CLOUD_CONFIG_BUNDLE_MAX_ATTEMPTS {
            tracing::warn!(
                status = ?status,
                attempt,
                max_attempts = CLOUD_CONFIG_BUNDLE_MAX_ATTEMPTS,
                "Failed to fetch cloud config bundle; retrying"
            );
            sleep(backoff(attempt as u64)).await;
            true
        } else {
            false
        }
    }

    async fn handle_unauthorized(
        &self,
        auth: &mut CodexAuth,
        auth_recovery: &mut UnauthorizedRecovery,
        trigger: &'static str,
        attempt: usize,
        status_code: Option<u16>,
        message: &str,
    ) -> Result<UnauthorizedRecoveryAction, CloudConfigBundleLoadError> {
        emit_fetch_attempt_metric(trigger, attempt, "unauthorized", status_code);
        if auth_recovery.has_next() {
            tracing::warn!(
                attempt,
                max_attempts = CLOUD_CONFIG_BUNDLE_MAX_ATTEMPTS,
                "Cloud config bundle request was unauthorized; attempting auth recovery"
            );
            match auth_recovery.next().await {
                Ok(_) => {
                    let Some(refreshed_auth) = self.auth_manager.auth().await else {
                        tracing::error!(
                            "Auth recovery succeeded but no auth is available for cloud config bundle"
                        );
                        emit_fetch_final_metric(
                            trigger,
                            "error",
                            "auth_recovery_missing_auth",
                            attempt,
                            status_code,
                            /*bundle*/ None,
                        );
                        return Err(CloudConfigBundleLoadError::new(
                            CloudConfigBundleLoadErrorCode::Auth,
                            status_code,
                            CLOUD_CONFIG_BUNDLE_AUTH_RECOVERY_FAILED_MESSAGE,
                        ));
                    };
                    *auth = refreshed_auth;
                    return Ok(UnauthorizedRecoveryAction::RetrySameAttempt);
                }
                Err(RefreshTokenError::Permanent(failed)) => {
                    tracing::warn!(
                        error = %failed,
                        "Failed to recover from unauthorized cloud config bundle request"
                    );
                    emit_fetch_final_metric(
                        trigger,
                        "error",
                        "auth_recovery_unrecoverable",
                        attempt,
                        status_code,
                        /*bundle*/ None,
                    );
                    return Err(CloudConfigBundleLoadError::new(
                        CloudConfigBundleLoadErrorCode::Auth,
                        status_code,
                        failed.message,
                    ));
                }
                Err(RefreshTokenError::Transient(recovery_err)) => {
                    if attempt < CLOUD_CONFIG_BUNDLE_MAX_ATTEMPTS {
                        tracing::warn!(
                            error = %recovery_err,
                            attempt,
                            max_attempts = CLOUD_CONFIG_BUNDLE_MAX_ATTEMPTS,
                            "Failed to recover from unauthorized cloud config bundle request; retrying"
                        );
                        sleep(backoff(attempt as u64)).await;
                    }
                    return Ok(UnauthorizedRecoveryAction::RetryNextAttempt);
                }
            }
        }

        tracing::warn!(
            error = %message,
            "Cloud config bundle request was unauthorized and no auth recovery is available"
        );
        emit_fetch_final_metric(
            trigger,
            "error",
            "auth_recovery_unavailable",
            attempt,
            status_code,
            /*bundle*/ None,
        );
        Err(CloudConfigBundleLoadError::new(
            CloudConfigBundleLoadErrorCode::Auth,
            status_code,
            CLOUD_CONFIG_BUNDLE_AUTH_RECOVERY_FAILED_MESSAGE,
        ))
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
