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
        reason = "forced refreshes share the existing per-provider cache lock with TTL refreshes"
    )]
    async fn refresh(&self, _context: ExternalAuthRefreshContext) -> io::Result<CodexAuth> {
        let started = Instant::now();
        let mut cached = self.state.cached_token.lock().await;
        if let Some(token) = cached.as_ref()
            && token.fetched_at > started
            && self
                .state
                .config
                .refresh_interval()
                .is_none_or(|interval| token.fetched_at.elapsed() < interval)
        {
            // Only reuse an acquisition completed while this refresh waited.
            // A subsequent rejection must still run the provider command.
            return Ok(CodexAuth::from_api_key(token.access_token.as_str()));
        }
        // Failed or cancelled refreshes must not expose the rejected token.
        *cached = None;
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
        // No raw helper stderr is surfaced; discard it without buffering it.
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(|err| {
        io::Error::other(format!("provider auth command `{}` failed to start: {err}", config.command))
    })?;
    let mut stdout = child.stdout.take().ok_or_else(|| io::Error::other("provider auth stdout was not captured"))?;
    // A bearer token is a bounded credential, not an arbitrary output artifact.
    // Keep stdout bytes exact and reject overflow atomically. kill_on_drop
    // retains the existing cleanup behavior on timeout/error/caller drop.
    const MAX_TOKEN_BYTES: u64 = 1024 * 1024;
    let (status, stdout) = tokio::time::timeout(config.timeout(), async {
        let mut bytes = Vec::new();
        (&mut stdout).take(MAX_TOKEN_BYTES + 1).read_to_end(&mut bytes).await?;
        if bytes.len() as u64 > MAX_TOKEN_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "provider auth token exceeds 1 MiB byte limit"));
        }
        let status = child.wait().await?;
        Ok::<_, io::Error>((status, bytes))
    })
        .await
        .map_err(|_| {
            io::Error::other(format!(
                "provider auth command `{}` timed out after {} ms",
                config.command,
                config.timeout_ms.get()
            ))
        })?
        .map_err(|err| {
            io::Error::other(format!(
                "provider auth command `{}` failed: {err}",
                config.command
            ))
        })?;

    if !status.success() {
        return Err(io::Error::other(format!(
            "provider auth command `{}` exited with status {status} (stderr withheld)",
            config.command
        )));
    }

    let stdout = String::from_utf8(stdout).map_err(|_| {
        io::Error::other(format!(
            "provider auth command `{}` wrote non-UTF-8 data to stdout",
            config.command
        ))
    })?;
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

