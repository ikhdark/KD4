use super::manager::CodexAuth;
use super::manager::ExternalAuth;
use super::manager::ExternalAuthFuture;
use super::manager::ExternalAuthRefreshContext;
use codex_protocol::config_types::ModelProviderAuthInfo;
use std::fmt;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

#[derive(Clone)]
pub(crate) struct BearerTokenRefresher {
    state: Arc<ExternalBearerAuthState>,
}

impl BearerTokenRefresher {
    pub(crate) fn new(config: ModelProviderAuthInfo) -> Self {
        Self {
            state: Arc::new(ExternalBearerAuthState::new(config)),
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "external bearer cache misses intentionally hold cached_token across the provider command to avoid duplicate refreshes"
    )]
    async fn resolve(&self) -> io::Result<CodexAuth> {
        let access_token = {
            let mut cached = self.state.cached_token.lock().await;
            if let Some(cached_token) = cached.as_ref() {
                let should_use_cached_token = match self.state.config.refresh_interval() {
                    Some(refresh_interval) => cached_token.fetched_at.elapsed() < refresh_interval,
                    None => true,
                };
                if should_use_cached_token {
                    return Ok(CodexAuth::from_api_key(cached_token.access_token.as_str()));
                }
            }

            let access_token = run_provider_auth_command(&self.state.config).await?;
            *cached = Some(CachedExternalBearerToken {
                access_token: access_token.clone(),
                fetched_at: Instant::now(),
            });
            access_token
        };
        Ok(CodexAuth::from_api_key(access_token.as_str()))
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "forced refresh shares the provider execution lock with cache misses"
    )]
    async fn refresh(&self, _context: ExternalAuthRefreshContext) -> io::Result<CodexAuth> {
        let mut cached = self.state.cached_token.lock().await;
        let access_token = run_provider_auth_command(&self.state.config).await?;
        *cached = Some(CachedExternalBearerToken {
            access_token: access_token.clone(),
            fetched_at: Instant::now(),
        });
        Ok(CodexAuth::from_api_key(access_token.as_str()))
    }
}

impl ExternalAuth for BearerTokenRefresher {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(BearerTokenRefresher::resolve(self))
    }

    fn refresh(&self, context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(BearerTokenRefresher::refresh(self, context))
    }
}

impl fmt::Debug for BearerTokenRefresher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BearerTokenRefresher")
            .finish_non_exhaustive()
    }
}

struct ExternalBearerAuthState {
    config: ModelProviderAuthInfo,
    cached_token: Mutex<Option<CachedExternalBearerToken>>,
}

impl ExternalBearerAuthState {
    fn new(config: ModelProviderAuthInfo) -> Self {
        Self {
            config,
            cached_token: Mutex::new(None),
        }
    }
}

struct CachedExternalBearerToken {
    access_token: String,
    fetched_at: Instant,
}

async fn run_provider_auth_command(config: &ModelProviderAuthInfo) -> io::Result<String> {
    let program = resolve_provider_auth_program(&config.command, &config.cwd)?;
    let mut command = Command::new(&program);
    command
        .args(&config.args)
        .current_dir(config.cwd.as_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|err| io::Error::other(format!("provider auth command failed to start: {err}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("provider stdout is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("provider stderr is unavailable"))?;
    let result = tokio::time::timeout(config.timeout(), async {
        tokio::try_join!(
            child.wait(),
            read_provider_output(stdout),
            read_provider_output(stderr)
        )
    })
    .await;
    let (status, stdout, _) = match result {
        Ok(Ok(output)) => output,
        outcome => {
            let _ = child.kill().await;
            return Err(match outcome {
                Ok(Err(error)) => error,
                _ => io::Error::other(format!(
                    "provider auth command timed out after {} ms",
                    config.timeout_ms.get()
                )),
            });
        }
    };
    if !status.success() {
        return Err(io::Error::other(format!(
            "provider auth command exited with status {status}"
        )));
    }
    let stdout = String::from_utf8(stdout)
        .map_err(|_| io::Error::other("provider auth command wrote non-UTF-8 data to stdout"))?;
    let access_token = stdout.trim().to_string();
    if access_token.is_empty() {
        return Err(io::Error::other(format!(
            "provider auth command `{}` produced an empty token",
            config.command
        )));
    }

    Ok(access_token)
}

fn resolve_provider_auth_program(command: &str, cwd: &Path) -> io::Result<PathBuf> {
    let path = Path::new(command);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    if path.components().count() > 1 {
        return Ok(cwd.join(path));
    }

    Ok(PathBuf::from(command))
}

async fn read_provider_output(reader: impl tokio::io::AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    reader
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(io::Error::other(
            "provider auth command exceeded the output limit",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::super::manager::ExternalAuthRefreshReason;
    use super::*;

    fn config(home: &Path, command: &str) -> ModelProviderAuthInfo {
        serde_json::from_value(serde_json::json!({
            "command": "cmd.exe", "args": ["/d", "/s", "/c", command],
            "cwd": home, "timeout_ms": 10000, "refresh_interval_ms": 60000
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn provider_failures_redact_stderr_and_bound_both_output_streams() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("output.txt"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        for command in ["type output.txt", "type output.txt >&2"] {
            let refresher = BearerTokenRefresher::new(config(home.path(), command));
            let error = refresher.resolve().await.unwrap_err();
            assert_eq!(
                error.to_string(),
                "provider auth command exceeded the output limit"
            );
        }
        let refresher =
            BearerTokenRefresher::new(config(home.path(), "echo stderr-secret >&2 & exit /b 7"));
        let error = refresher.resolve().await.unwrap_err().to_string();
        assert!(error.contains("exited with status"));
        assert!(!error.contains("stderr-secret"));
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Holding the provider lock proves refresh cannot execute before admission"
    )]
    async fn forced_refresh_waits_for_provider_execution_lock() {
        let home = tempfile::tempdir().unwrap();
        let refresher = BearerTokenRefresher::new(config(
            home.path(),
            "echo started>started.txt & echo refreshed-token",
        ));
        let cached = refresher.state.cached_token.lock().await;
        let refresh = refresher.refresh(ExternalAuthRefreshContext {
            reason: ExternalAuthRefreshReason::Unauthorized,
            previous_account_id: None,
        });
        tokio::pin!(refresh);
        tokio::select! {
            result = &mut refresh => panic!("refresh completed while execution lock held: {result:?}"),
            () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
        }
        assert!(!home.path().join("started.txt").exists());
        drop(cached);
        assert_eq!(refresh.await.unwrap().api_key(), Some("refreshed-token"));
        assert!(home.path().join("started.txt").exists());
        assert_eq!(
            refresher.resolve().await.unwrap().api_key(),
            Some("refreshed-token")
        );
    }
}
