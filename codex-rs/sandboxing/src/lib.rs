mod denial;
mod manager;
pub mod policy_transforms;
mod windows;

pub use codex_windows_sandbox::WindowsSandboxProxySettingsMode;
pub use denial::is_likely_sandbox_denied;
pub use manager::SandboxCommand;
pub use manager::SandboxDirectSpawnTransformRequest;
pub use manager::SandboxExecRequest;
pub use manager::SandboxTransformError;
pub use manager::SandboxTransformRequest;
pub use manager::SandboxType;
pub use manager::SandboxablePreference;
pub use manager::compatibility_sandbox_policy_for_permission_profile;
pub use manager::get_platform_sandbox;
pub use manager::select_initial;
pub use manager::should_sandbox;
pub use manager::transform;
pub use manager::transform_for_direct_spawn;
pub use manager::with_managed_mitm_ca_readable_root;
pub use windows::WindowsSandboxFilesystemOverrides;
pub use windows::permission_profile_supports_windows_restricted_token_sandbox;
pub use windows::resolve_windows_elevated_filesystem_overrides;
pub use windows::resolve_windows_restricted_token_filesystem_overrides;
pub use windows::unsupported_windows_restricted_token_sandbox_reason;
pub use windows::windows_sandbox_uses_elevated_backend;

use codex_protocol::error::CodexErr;

impl From<SandboxTransformError> for CodexErr {
    fn from(err: SandboxTransformError) -> Self {
        match err {
            error @ SandboxTransformError::InvalidCommandCwd { .. }
            | error @ SandboxTransformError::InvalidSandboxPolicyCwd { .. } => {
                CodexErr::InvalidRequest(error.to_string())
            }
            SandboxTransformError::EnvironmentNetworkProxy(message) => {
                CodexErr::UnsupportedOperation(message)
            }
            SandboxTransformError::WindowsSandboxPreparation(message) => {
                CodexErr::UnsupportedOperation(message)
            }
        }
    }
}
