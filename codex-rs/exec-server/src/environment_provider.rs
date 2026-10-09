use std::future::Future;
use std::pin::Pin;

use crate::Environment;
use crate::ExecServerError;
use crate::environment::CODEX_EXEC_SERVER_URL_ENV_VAR;
use crate::environment::LOCAL_ENVIRONMENT_ID;
use crate::environment::REMOTE_ENVIRONMENT_ID;

/// Lists the concrete environments available to Codex.
///
/// Implementations own a startup snapshot containing both the available
/// environment list in configured order and the default environment
/// selection. Providers should only return provider-owned remote environments;
/// `include_local` controls whether `EnvironmentManager` should add the local
/// environment to the snapshot.
pub trait EnvironmentProvider: Send + Sync {
    /// Returns the provider-owned environment startup snapshot.
    fn snapshot(&self) -> EnvironmentProviderFuture<'_>;
}

pub type EnvironmentProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<EnvironmentProviderSnapshot, ExecServerError>> + Send + 'a>>;

#[derive(Clone, Debug)]
pub struct EnvironmentProviderSnapshot {
    pub environments: Vec<(String, Environment)>,
    pub default: EnvironmentDefault,
    pub include_local: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvironmentDefault {
    Disabled,
    EnvironmentId(String),
}

/// Default provider backed by `CODEX_EXEC_SERVER_URL`.
#[derive(Clone, Debug)]
pub struct DefaultEnvironmentProvider {
    exec_server_url: Option<String>,
}

impl DefaultEnvironmentProvider {
    /// Builds a provider from an already-read raw `CODEX_EXEC_SERVER_URL` value.
    pub fn new(exec_server_url: Option<String>) -> Self {
        Self { exec_server_url }
    }

    /// Builds a provider by reading `CODEX_EXEC_SERVER_URL`.
    pub fn from_env() -> Self {
        Self::new(std::env::var(CODEX_EXEC_SERVER_URL_ENV_VAR).ok())
    }

    pub(crate) fn snapshot_inner(&self) -> EnvironmentProviderSnapshot {
        let mut environments = Vec::new();
        let (exec_server_url, disabled) = normalize_exec_server_url(self.exec_server_url.clone());

        if let Some(exec_server_url) = exec_server_url {
            environments.push((
                REMOTE_ENVIRONMENT_ID.to_string(),
                Environment::remote_inner(exec_server_url, /*local_runtime_paths*/ None),
            ));
        }

        let has_remote = environments
            .iter()
            .any(|(id, _environment)| id == REMOTE_ENVIRONMENT_ID);
        let include_local = !disabled && !has_remote;
        let default = if disabled {
            EnvironmentDefault::Disabled
        } else if has_remote {
            EnvironmentDefault::EnvironmentId(REMOTE_ENVIRONMENT_ID.to_string())
        } else {
            EnvironmentDefault::EnvironmentId(LOCAL_ENVIRONMENT_ID.to_string())
        };

        EnvironmentProviderSnapshot {
            environments,
            default,
            include_local,
        }
    }
}

impl EnvironmentProvider for DefaultEnvironmentProvider {
    fn snapshot(&self) -> EnvironmentProviderFuture<'_> {
        Box::pin(async { Ok(self.snapshot_inner()) })
    }
}

pub(crate) fn normalize_exec_server_url(exec_server_url: Option<String>) -> (Option<String>, bool) {
    match exec_server_url.as_deref().map(str::trim) {
        None | Some("") => (None, false),
        Some(url) if url.eq_ignore_ascii_case("none") => (None, true),
        Some(url) => (Some(url.to_string()), false),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn default_provider_normalizes_urls_and_selects_complete_snapshot() {
        for (url, expected_remote, disabled) in [
            (None, None, false),
            (Some(""), None, false),
            (Some(" \t "), None, false),
            (Some("none"), None, true),
            (Some(" NoNe "), None, true),
            (Some("ws://127.0.0.1:8765"), Some("ws://127.0.0.1:8765"), false),
            (Some(" ws://127.0.0.1:8765 "), Some("ws://127.0.0.1:8765"), false),
        ] {
            let snapshot = DefaultEnvironmentProvider::new(url.map(str::to_string))
                .snapshot().await.expect("environments");
            assert_eq!(snapshot.include_local, !disabled && expected_remote.is_none());
            assert_eq!(
                snapshot.default,
                if disabled {
                    EnvironmentDefault::Disabled
                } else {
                    EnvironmentDefault::EnvironmentId(
                        if expected_remote.is_some() { REMOTE_ENVIRONMENT_ID } else { LOCAL_ENVIRONMENT_ID }.to_string()
                    )
                }
            );
            assert_eq!(snapshot.environments.len(), usize::from(expected_remote.is_some()));
            if let Some(expected_remote) = expected_remote {
                let (id, environment) = &snapshot.environments[0];
                assert_eq!(id, REMOTE_ENVIRONMENT_ID);
                assert!(environment.is_remote());
                assert_eq!(environment.exec_server_url(), Some(expected_remote));
            }
        }
    }
}
