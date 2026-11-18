use axum::http::HeaderMap;
use axum::http::HeaderValue;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::UnauthorizedRecovery;
use std::future::Future;
use std::future::poll_fn;
use std::io;
use std::io::ErrorKind;
use std::sync::Arc;
use std::task::Poll;
use tokio::sync::watch;
use tracing::info;
use tracing::warn;

pub(super) const REMOTE_CONTROL_ACCOUNT_ID_HEADER: &str = "chatgpt-account-id";
// Initial attempt plus the managed Reload and RefreshToken recovery steps.
pub(super) const REMOTE_CONTROL_AUTH_ATTEMPTS: usize = 3;

pub(super) struct RemoteControlConnectionAuth {
    pub(super) account_id: String,
    auth: CodexAuth,
    manager: Arc<AuthManager>,
    epoch: u64,
    revision: u64,
}

impl RemoteControlConnectionAuth {
    pub(super) fn request_headers(&self) -> io::Result<HeaderMap> {
        self.with_current_auth(
            "remote control credentials changed before request preparation",
            |current| self.build_request_headers(current),
        )
    }

    pub(super) async fn dispatch_request<F: Future>(&self, send: F) -> io::Result<F::Output> {
        let mut send = std::pin::pin!(send);
        let mut first_poll = true;
        poll_fn(|context| {
            if std::mem::take(&mut first_poll) {
                // Admit the exact auth revision that supplied the headers. Only the first
                // poll runs under the auth guard; a pending response must not block logout.
                match self.with_current_auth(
                    "remote control credentials changed before request dispatch",
                    |_| Ok(send.as_mut().poll(context)),
                ) {
                    Ok(result) => result.map(Ok),
                    Err(error) => Poll::Ready(Err(error)),
                }
            } else {
                send.as_mut().poll(context).map(Ok)
            }
        })
        .await
    }

    fn with_current_auth<T>(
        &self,
        revision_error: &'static str,
        use_auth: impl FnOnce(&CodexAuth) -> io::Result<T>,
    ) -> io::Result<T> {
        self.manager.with_usable_auth(self.epoch, |current, revision, _| {
            if revision != self.revision {
                return Err(io::Error::new(ErrorKind::WouldBlock, revision_error));
            }
            use_auth(current)
        })?
    }

    fn build_request_headers(&self, auth: &CodexAuth) -> io::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        codex_model_provider::auth_provider_from_auth(auth)
            .try_add_auth_headers(&mut headers)
            .map_err(|error| {
                let kind = match &error {
                    codex_api::AuthError::Build(_) => ErrorKind::InvalidInput,
                    codex_api::AuthError::Transient(_) => ErrorKind::WouldBlock,
                };
                io::Error::new(kind, error)
            })?;
        headers.insert(
            REMOTE_CONTROL_ACCOUNT_ID_HEADER,
            HeaderValue::from_str(&self.account_id).map_err(|err| {
                io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("invalid remote control account id header: {err}"),
                )
            })?,
        );
        Ok(headers)
    }
}

pub(super) async fn load_remote_control_auth(
    auth_manager: &Arc<AuthManager>,
) -> io::Result<RemoteControlConnectionAuth> {
    let mut reloaded = false;
    let auth = loop {
        let Some(auth) = auth_manager.auth().await else {
            if reloaded {
                return Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    "remote control requires ChatGPT authentication",
                ));
            }
            auth_manager.reload().await;
            reloaded = true;
            continue;
        };
        if !auth.uses_codex_backend() {
            break auth;
        }
        if auth.get_account_id().is_none() && !reloaded {
            auth_manager.reload().await;
            reloaded = true;
            continue;
        }
        break auth;
    };

    if !auth.uses_codex_backend() {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "remote control requires ChatGPT authentication; API key auth is not supported",
        ));
    }

    let (epoch, _) = auth_manager.policy_auth_snapshot()?;
    let revision = auth_manager.auth_revision_for_snapshot(&auth).ok_or_else(|| {
        io::Error::new(ErrorKind::WouldBlock, "remote control credentials changed while loading")
    })?;
    auth_manager.with_usable_auth(epoch, |_, current_revision, _| {
        if revision != current_revision {
            return Err(io::Error::new(
                ErrorKind::WouldBlock,
                "remote control credentials changed while loading",
            ));
        }
        Ok(RemoteControlConnectionAuth {
            account_id: auth.get_account_id().ok_or_else(|| {
                io::Error::new(
                    ErrorKind::WouldBlock,
                    "remote control enrollment is waiting for a ChatGPT account id",
                )
            })?,
            auth,
            manager: Arc::clone(auth_manager),
            epoch,
            revision,
        })
    })?
}

pub(super) async fn recover_remote_control_auth(
    auth_recovery: &mut UnauthorizedRecovery,
    auth_change_rx: &mut watch::Receiver<u64>,
    rejected_auth: &RemoteControlConnectionAuth,
) -> bool {
    // A no-change reload did not repair the rejected credential. Advance directly to
    // refresh instead of spending another HTTP attempt on the identical snapshot.
    for _ in 0..REMOTE_CONTROL_AUTH_ATTEMPTS - 1 {
        if !auth_recovery.has_next() {
            return false;
        }

        let mode = auth_recovery.mode_name();
        let step = auth_recovery.step_name();
        let (auth_change_revision_before_recovery, unread_before_recovery) = {
            let revision = auth_change_rx.borrow();
            (*revision, auth_change_rx.has_changed().unwrap_or(true))
        };
        match auth_recovery.next_with_auth(&rejected_auth.auth).await {
            Ok(step_result) => {
                if step_result.auth_state_changed() == Some(true) {
                    mark_recovery_auth_change_seen(
                        auth_change_rx,
                        auth_change_revision_before_recovery,
                        unread_before_recovery,
                    );
                }
                info!(
                    "remote control auth recovery succeeded: mode={mode}, step={step}, auth_state_changed={:?}",
                    step_result.auth_state_changed()
                );
                if step_result.auth_state_changed() == Some(false) {
                    continue;
                }
                return true;
            }
            Err(err) => {
                warn!("remote control auth recovery failed: mode={mode}, step={step}: {err}");
                return false;
            }
        }
    }
    false
}

pub(super) fn mark_recovery_auth_change_seen(
    auth_change_rx: &mut watch::Receiver<u64>,
    auth_change_revision_before_recovery: u64,
    unread_before_recovery: bool,
) {
    let auth_change_revision_after_recovery = *auth_change_rx.borrow_and_update();
    if unread_before_recovery
        || auth_change_revision_after_recovery != auth_change_revision_before_recovery.wrapping_add(1)
    {
        // Recovery updated the same watch that wakes the outer reconnect
        // loop. Mark only that single revision seen; if more revisions
        // arrived while recovery was in flight, leave them pending so the
        // reconnect loop still reacts to the later external auth change.
        auth_change_rx.mark_changed();
    }
}

