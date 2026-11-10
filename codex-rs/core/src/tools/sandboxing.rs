//! Shared approvals and sandboxing traits used by tool runtimes.
//!
//! Consolidates the approval flow primitives (`ApprovalDecision`, `ApprovalStore`,
//! `ApprovalCtx`, `Approvable`) together with the sandbox orchestration traits
//! and helpers (`ToolRuntime`, `SandboxAttempt`, etc.).

use crate::sandboxing::ExecOptions;
use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::SessionServices;
use crate::tools::hook_names::HookToolName;
use codex_file_system::FileSystemSandboxContext;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::approvals::ExecPolicyAmendment;
use codex_protocol::approvals::NetworkApprovalContext;
use codex_protocol::error::CodexErr;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxKind;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::request_permissions::UriAdditionalPermissionProfile;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxTransformRequest;
use codex_sandboxing::SandboxType;
use codex_sandboxing::policy_transforms::effective_permission_profile_uri;
use codex_sandboxing::transform;
use codex_tools::ToolName;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::Future;
use futures::future::BoxFuture;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;

/// Shared by approval, provenance rechecks and actual launch. Worker commands
/// use the inherited turn permissions without a separate sandbox override.
pub(crate) fn command_permission_profile(turn: &TurnContext) -> PermissionProfile {
    turn.permission_profile()
}

pub(crate) fn command_windows_sandbox_level(turn: &TurnContext) -> WindowsSandboxLevel {
    turn.windows_sandbox_level
}

#[derive(Clone, Default, Debug)]
pub(crate) struct ApprovalStore {
    // Store serialized keys for generic caching across requests.
    map: HashMap<String, ReviewDecision>,
}

impl ApprovalStore {
    fn serialized_key<K>(tool_name: &str, key: &K) -> Option<String>
    where
        K: Serialize,
    {
        serde_json::to_string(&(1_u8, tool_name, std::any::type_name::<K>(), key)).ok()
    }

    pub fn get<K>(&self, tool_name: &str, key: &K) -> Option<ReviewDecision>
    where
        K: Serialize,
    {
        let s = Self::serialized_key(tool_name, key)?;
        self.map.get(&s).cloned()
    }

    pub fn put<K>(&mut self, tool_name: &str, key: K, value: ReviewDecision)
    where
        K: Serialize,
    {
        if let Some(s) = Self::serialized_key(tool_name, &key) {
            self.map.insert(s, value);
        }
    }
}

/// Takes a vector of approval keys and returns a ReviewDecision.
/// There will be one key in most cases, but apply_patch can modify multiple files at once.
///
/// - If all keys are already approved for session, we skip prompting.
/// - If the user approves for session, we store the decision for each key individually
///   so future requests touching any subset can also skip prompting.
pub(crate) async fn with_cached_approval<K, F, Fut>(
    services: &SessionServices,
    // Name of the tool, used for metrics collection.
    tool_name: &str,
    keys: Vec<K>,
    fetch: F,
) -> ReviewDecision
where
    K: Serialize,
    F: FnOnce() -> Fut,
    Fut: Future<Output = ReviewDecision>,
{
    // To be defensive here, don't bother with checking the cache if keys are empty.
    if keys.is_empty() {
        return fetch().await;
    }

    // Reuse each serialized key if the user grants session approval. Keep failed
    // serializations as misses so they can never accidentally authorize a call.
    let serialized_keys = keys
        .iter()
        .map(|key| ApprovalStore::serialized_key(tool_name, key))
        .collect::<Vec<_>>();
    let already_approved = {
        let store = services.tool_approvals.lock().await;
        serialized_keys.iter().all(|key| {
            matches!(
                key.as_ref().and_then(|key| store.map.get(key)),
                Some(ReviewDecision::ApprovedForSession)
            )
        })
    };

    if already_approved {
        return ReviewDecision::ApprovedForSession;
    }

    let decision = fetch().await;

    services.session_telemetry.counter(
        "codex.approval.requested",
        /*inc*/ 1,
        &[
            ("tool", tool_name),
            ("approved", decision.to_opaque_string()),
        ],
    );

    if matches!(decision, ReviewDecision::ApprovedForSession) {
        let mut store = services.tool_approvals.lock().await;
        for key in serialized_keys.into_iter().flatten() {
            store.map.insert(key, ReviewDecision::ApprovedForSession);
        }
    }

    decision
}

#[derive(Clone)]
pub(crate) struct ApprovalCtx<'a> {
    pub session: &'a Arc<Session>,
    pub turn: &'a Arc<TurnContext>,
    pub call_id: &'a str,
    pub retry_reason: Option<String>,
    pub network_approval_context: Option<NetworkApprovalContext>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PermissionRequestPayload {
    pub tool_name: HookToolName,
    pub tool_input: serde_json::Value,
}

impl PermissionRequestPayload {
    pub(crate) fn bash(command: String, description: Option<String>) -> Self {
        let mut tool_input = serde_json::Map::new();
        tool_input.insert("command".to_string(), serde_json::Value::String(command));
        if let Some(description) = description {
            tool_input.insert(
                "description".to_string(),
                serde_json::Value::String(description),
            );
        }

        Self {
            tool_name: HookToolName::bash(),
            tool_input: serde_json::Value::Object(tool_input),
        }
    }
}

// Specifies what tool orchestrator should do with a given tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExecApprovalRequirement {
    /// No approval required for this tool call.
    Skip {
        /// The first attempt should skip sandboxing (e.g., when explicitly
        /// greenlit by policy).
        bypass_sandbox: bool,
        /// Proposed execpolicy amendment to skip future approvals for similar commands
        /// Only applies if the command fails to run in sandbox and codex prompts the user to run outside the sandbox.
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
    },
    /// Approval required for this tool call.
    NeedsApproval {
        reason: Option<String>,
        /// Proposed execpolicy amendment to skip future approvals for similar commands
        /// See core/src/exec_policy.rs for more details on how proposed_execpolicy_amendment is determined.
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
    },
    /// Execution forbidden for this tool call.
    Forbidden { reason: String },
}

impl ExecApprovalRequirement {
    pub fn proposed_execpolicy_amendment(&self) -> Option<&ExecPolicyAmendment> {
        match self {
            Self::NeedsApproval {
                proposed_execpolicy_amendment: Some(prefix),
                ..
            } => Some(prefix),
            Self::Skip {
                proposed_execpolicy_amendment: Some(prefix),
                ..
            } => Some(prefix),
            _ => None,
        }
    }
}

pub(crate) fn same_exec_authorization_envelope(
    original: &ExecApprovalRequirement,
    canonical: &ExecApprovalRequirement,
) -> bool {
    // Fail closed on every policy-visible detail, including approval reasons and proposed policy
    // amendments. The canonical target is an internal compatibility check only; the original
    // PowerShell requirement remains the one consumed by the execution path.
    original == canonical
}

/// - Never: do not ask
/// - OnRequest: ask unless filesystem access is unrestricted
/// - Granular: ask unless filesystem access is unrestricted, but auto-reject
///   when granular sandbox approval is disabled.
/// - UnlessTrusted: always ask
pub(crate) fn default_exec_approval_requirement(
    policy: AskForApproval,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
) -> ExecApprovalRequirement {
    let needs_approval = match policy {
        AskForApproval::Never => false,
        AskForApproval::OnRequest | AskForApproval::Granular(_) => {
            matches!(
                file_system_sandbox_policy.kind,
                FileSystemSandboxKind::Restricted
            )
        }
        AskForApproval::UnlessTrusted => true,
    };

    if needs_approval
        && matches!(
            policy,
            AskForApproval::Granular(granular_config)
                if !granular_config.allows_sandbox_approval()
        )
    {
        ExecApprovalRequirement::Forbidden {
            reason: "approval policy disallowed sandbox approval prompt".to_string(),
        }
    } else if needs_approval {
        ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: None,
        }
    } else {
        ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SandboxOverride {
    NoOverride,
    BypassSandboxFirstAttempt,
}

pub(crate) fn sandbox_override_for_first_attempt(
    sandbox_permissions: SandboxPermissions,
    exec_approval_requirement: &ExecApprovalRequirement,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
) -> SandboxOverride {
    // Deny-read restrictions are part of the active permission policy. Running
    // without a filesystem sandbox would discard them, even if the command was
    // otherwise approved by rules or explicit escalation.
    if !unsandboxed_execution_allowed(file_system_sandbox_policy) {
        return SandboxOverride::NoOverride;
    }

    // ExecPolicy `Allow` can intentionally imply full trust (Skip + bypass_sandbox=true),
    // which supersedes `with_additional_permissions` sandboxed execution hints.
    if matches!(
        exec_approval_requirement,
        ExecApprovalRequirement::Skip {
            bypass_sandbox: true,
            ..
        }
    ) {
        return SandboxOverride::BypassSandboxFirstAttempt;
    }

    if sandbox_permissions.requires_escalated_permissions() {
        SandboxOverride::BypassSandboxFirstAttempt
    } else {
        SandboxOverride::NoOverride
    }
}

/// Returns true when the active filesystem policy can be represented by
/// running without a filesystem sandbox.
///
/// Denied reads only exist inside the sandbox. If a policy contains any
/// denied-read paths, bypassing the sandbox would silently grant those reads,
/// so escalation must keep the command sandboxed with the denied reads intact.
pub(crate) fn unsandboxed_execution_allowed(
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
) -> bool {
    !file_system_sandbox_policy.has_denied_read_restrictions()
}


pub(crate) trait Approvable<Req> {
    type ApprovalKey: Hash + Eq + Clone + Debug + Serialize;

    // In most cases (shell, unified_exec), a request will have a single approval key.
    //
    // However, apply_patch needs session "Allow, don't ask again" semantics that
    // apply to multiple atomic targets (e.g., apply_patch approves per file path). Returning
    // a list of keys lets the runtime treat the request as approved-for-session only if
    // *all* keys are already approved, while still caching approvals per-key so future
    // requests touching a subset can be auto-approved.
    fn approval_keys(&self, req: &Req) -> Vec<Self::ApprovalKey>;

    /// Return per-request sandbox permissions for first-attempt sandbox
    /// selection. Most tools use the ambient sandbox policy unchanged.
    fn sandbox_permissions(&self, _req: &Req) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    /// Return `Some(_)` to specify a custom exec approval requirement, or `None`
    /// to fall back to policy-based default.
    fn exec_approval_requirement(&self, _req: &Req) -> Option<ExecApprovalRequirement> {
        None
    }

    /// Return hook input for approval-time policy hooks when this runtime wants
    /// hook evaluation to run before user approval.
    fn permission_request_payload(&self, _req: &Req) -> Option<PermissionRequestPayload> {
        None
    }

    fn start_approval_async<'a>(
        &'a mut self,
        req: &'a Req,
        ctx: ApprovalCtx<'a>,
    ) -> BoxFuture<'a, ReviewDecision>;
}

pub(crate) struct ToolCtx {
    pub session: Arc<Session>,
    pub turn: Arc<TurnContext>,
    pub call_id: String,
    pub tool_name: ToolName,
}

#[derive(Debug)]
pub(crate) enum ToolError {
    /// A deliberate user, reviewer, hook, or policy denial.
    Denied(String),
    /// An operational failure that prevents the tool request from running.
    Rejected(String),
    Codex(CodexErr),
}

pub(crate) trait ToolRuntime<Req, Out>: Approvable<Req> {
    fn sandbox_cwd<'a>(&self, _req: &'a Req) -> Option<&'a PathUri> {
        None
    }

    async fn run(
        &mut self,
        req: &Req,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<Out, ToolError>;
}

pub(crate) struct SandboxAttempt<'a> {
    pub(crate) codex_home: &'a AbsolutePathBuf,
    pub sandbox: SandboxType,
    /// Whether policy requested sandboxing, independent of this host's concrete wrapper.
    pub sandbox_requested: bool,
    pub permissions: &'a codex_protocol::models::PermissionProfile,
    /// Canonical permissions before this host materializes workspace roots.
    pub exec_server_permissions: &'a codex_protocol::models::PermissionProfile,
    pub(crate) sandbox_cwd: &'a PathUri,
    pub(crate) workspace_roots: &'a [AbsolutePathBuf],
    pub windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel,
    pub windows_sandbox_private_desktop: bool,
}

impl<'a> SandboxAttempt<'a> {

    pub async fn env_for(
        &self,
        command: SandboxCommand,
        options: ExecOptions,
    ) -> Result<crate::sandboxing::ExecRequest, CodexErr> {
        let execution = crate::tools::tool_dispatch_trace::active_tool_dispatch_timing()
            .map(|timing| timing.execution_id().0.clone()).unwrap_or_default();
        let observation = codex_protocol::timing::observations::Span::start(
            "sandbox_preparation", "", "", &execution,
            serde_json::json!({"boundary":"queue_through_prepared_request"}));
        let permissions = self.permissions.clone();
        let sandbox = self.sandbox;
        let sandbox_cwd = self.sandbox_cwd.clone();
        let windows_sandbox_level = self.windows_sandbox_level;
        let windows_sandbox_private_desktop = self.windows_sandbox_private_desktop;
        let workspace_roots = self.workspace_roots.to_vec();
        let codex_home = self.codex_home.clone();
        let result = tokio::task::spawn_blocking(move || {
            let worker = codex_protocol::timing::observations::Span::start(
                "sandbox_transform", "", "", &execution,
                serde_json::json!({"boundary":"worker_only_excludes_queue_and_process_spawn"}));
            let result = (|| {
            let request = transform(SandboxTransformRequest {
                command,
                permissions: &permissions,
                sandbox,
                sandbox_policy_cwd: &sandbox_cwd,
                windows_sandbox_level,
                windows_sandbox_private_desktop,
            })
            .map_err(CodexErr::from)?;
            crate::sandboxing::ExecRequest::from_sandbox_exec_request(
                request,
                options,
                workspace_roots,
                codex_home,
            )
            })();
            worker.finish(if result.is_ok() {"success"} else {"failed"}, serde_json::Value::Null);
            result
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("sandbox preparation worker failed: {error}")));
        observation.finish(if matches!(&result, Ok(Ok(_))) {"success"} else {"failed"}, serde_json::Value::Null);
        result?
    }

    pub fn env_for_exec_server(
        &self,
        command: SandboxCommand,
        options: ExecOptions,
        additional_permissions_uri: Option<&UriAdditionalPermissionProfile>,
    ) -> Result<crate::sandboxing::ExecRequest, CodexErr> {
        let execution = crate::tools::tool_dispatch_trace::active_tool_dispatch_timing()
            .map(|timing| timing.execution_id().0.clone()).unwrap_or_default();
        let observation = codex_protocol::timing::observations::Span::start(
            "sandbox_remote_preparation", "", "", &execution,
            serde_json::json!({"boundary":"local_envelope_only_not_remote_sandbox"}));
        let exec_server_permissions = effective_permission_profile_uri(
            self.exec_server_permissions,
            additional_permissions_uri,
        );
        let request = transform(SandboxTransformRequest {
            command,
            permissions: self.permissions,
            // The exec-server must receive the native command, not this host's wrapper.
            sandbox: SandboxType::None,
            sandbox_policy_cwd: self.sandbox_cwd,
            windows_sandbox_level: self.windows_sandbox_level,
            windows_sandbox_private_desktop: self.windows_sandbox_private_desktop,
        })
        .map_err(CodexErr::from)?;
        let mut exec_request = crate::sandboxing::ExecRequest::from_sandbox_exec_request(
            request,
            options,
            self.workspace_roots.to_vec(),
            self.codex_home.clone(),
        )?;
        if self.sandbox_requested {
            exec_request.exec_server_sandbox = Some(FileSystemSandboxContext {
                permissions: exec_server_permissions,
                cwd: Some(exec_request.windows_sandbox_policy_cwd.clone()),
                workspace_roots: Vec::new(),
                windows_sandbox_level: self.windows_sandbox_level,
                windows_sandbox_private_desktop: self.windows_sandbox_private_desktop,
            });
        }
        observation.finish("success", serde_json::Value::Null);
        Ok(exec_request)
    }
}
