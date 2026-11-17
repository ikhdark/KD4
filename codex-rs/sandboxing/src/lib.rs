mod manager;
pub mod policy_transforms;

pub use manager::WindowsSandboxProxySettingsMode;
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
pub use manager::transform_for_direct_spawn_with_codex_home;

use codex_protocol::error::CodexErr;

impl From<SandboxTransformError> for CodexErr {
    fn from(err: SandboxTransformError) -> Self {
        match err {
            error @ SandboxTransformError::InvalidCommandCwd { .. }
            | error @ SandboxTransformError::InvalidSandboxPolicyCwd { .. } => {
                CodexErr::InvalidRequest(error.to_string())
            }
            SandboxTransformError::WindowsSandboxPreparation(message) => {
                CodexErr::UnsupportedOperation(message)
            }
        }
    }
}
