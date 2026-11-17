use super::AbsolutePathBuf;
use super::Arc;
use super::ClientResponsePayload;
use super::Config;
use super::ConfigManager;
use super::ConnectionRequestId;
use super::JSONRPCErrorError;
use super::OutgoingMessageSender;
use super::Path;
use super::PathBuf;
use super::Result;
use super::WindowsSandboxGrantReadRootParams;
use super::WindowsSandboxGrantReadRootResponse;
use super::WindowsSandboxReadiness;
use super::WindowsSandboxReadinessResponse;
use super::WindowsSandboxSetupStartParams;
use super::WindowsSandboxSetupStartResponse;
use super::internal_error;
use super::invalid_request;

/// Compatibility endpoints for clients that still send Windows sandbox RPCs.
/// KD4 runs with full access: no endpoint provisions a sandbox or changes ACLs.
#[derive(Clone)]
pub(crate) struct WindowsSandboxRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
}

impl WindowsSandboxRequestProcessor {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        _config: Arc<Config>,
        _config_manager: ConfigManager,
    ) -> Self {
        Self { outgoing }
    }

    pub(crate) async fn windows_sandbox_readiness(
        &self,
    ) -> Result<WindowsSandboxReadinessResponse, JSONRPCErrorError> {
        Ok(WindowsSandboxReadinessResponse {
            status: WindowsSandboxReadiness::Unsupported,
        })
    }

    pub(crate) async fn windows_sandbox_grant_read_root(
        &self,
        params: WindowsSandboxGrantReadRootParams,
    ) -> Result<WindowsSandboxGrantReadRootResponse, JSONRPCErrorError> {
        // Preserve the old full-access behavior: validate the directory and
        // return its canonical path without granting any filesystem permissions.
        let read_root = PathBuf::from(params.path);
        let canonical_path = tokio::task::spawn_blocking(move || {
            validate_and_canonicalize_read_root(&read_root)
        })
        .await
        .map_err(|err| internal_error(format!("Windows sandbox read-root task failed: {err}")))?
        .map_err(|err| {
            invalid_request(format!("failed to validate Windows sandbox read root: {err}"))
        })?;
        let path = AbsolutePathBuf::try_from(canonical_path).map_err(|err| {
            internal_error(format!("read root is not absolute: {err}"))
        })?;
        Ok(WindowsSandboxGrantReadRootResponse { path })
    }

    pub(crate) async fn windows_sandbox_setup_start(
        &self,
        request_id: &ConnectionRequestId,
        params: WindowsSandboxSetupStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if params.permission_profile_id.is_some() && params.permission_profile.is_some() {
            return Err(invalid_request(
                "permissionProfileId and permissionProfile are mutually exclusive",
            ));
        }
        // The request remains understood, but setup is no longer supported.
        // Do not claim it started or schedule a completion/provisioning task.
        self.outgoing
            .send_response(
                request_id.clone(),
                WindowsSandboxSetupStartResponse { started: false },
            )
            .await;
        Ok(None)
    }
}

fn validate_and_canonicalize_read_root(read_root: &Path) -> anyhow::Result<PathBuf> {
    if !read_root.is_absolute() {
        anyhow::bail!("path must be absolute: {}", read_root.display());
    }
    let metadata = std::fs::metadata(read_root)?;
    if !metadata.is_dir() {
        anyhow::bail!("path must be a directory: {}", read_root.display());
    }
    Ok(std::fs::canonicalize(read_root)?)
}
