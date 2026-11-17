use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::user_input::UserInput;
use codex_utils_path_uri::PathUri;

/// Host-owned turn environment summary visible to turn-input contributors.
#[derive(Debug, Clone)]
pub struct TurnInputEnvironment {
    /// Stable host environment id used to route executor-scoped capabilities.
    pub environment_id: String,
    /// Effective working directory for this turn in the environment.
    pub cwd: PathUri,
    /// Whether this is the primary environment for the turn.
    pub is_primary: bool,
}

/// The turn's current filesystem permissions for one turn environment.
///
/// Extensions that read environment-owned files pass this sandbox to that environment's
/// filesystem. It only narrows access to the turn's permissions; it never grants access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentFileSystemSandbox {
    pub environment_id: String,
    pub sandbox: codex_exec_server::FileSystemSandboxContext,
}

/// Returns the current-turn sandbox for `environment_id`, if the host supplied one.
pub fn environment_file_system_sandbox<'a>(
    sandboxes: &'a [EnvironmentFileSystemSandbox],
    environment_id: &str,
) -> Option<&'a codex_exec_server::FileSystemSandboxContext> {
    sandboxes
        .iter()
        .find(|sandbox| sandbox.environment_id == environment_id)
        .map(|sandbox| &sandbox.sandbox)
}

/// Turn facts supplied before the host records turn-local model input items.
///
/// The host owns this snapshot and lends it to each turn-input contributor for the duration of
/// dispatch. Contributors should clone only the fields they need to retain beyond that call.
#[derive(Debug, Clone)]
pub struct TurnInputContext {
    /// Stable host-owned turn identifier.
    pub turn_id: String,
    /// User input submitted for this turn.
    pub user_input: Vec<UserInput>,
    /// Resolved turn environments, in host priority order.
    pub environments: Vec<TurnInputEnvironment>,
    /// Selected capability roots that are ready in this step.
    pub ready_selected_capability_roots: Vec<SelectedCapabilityRoot>,
    pub bound_selected_capability_roots: Vec<codex_exec_server::ResolvedSelectedCapabilityRoot>,
    /// Current filesystem permissions for each turn environment.
    pub file_system_sandboxes: Vec<EnvironmentFileSystemSandbox>,
}
