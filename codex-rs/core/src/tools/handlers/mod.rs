pub(crate) mod agent_jobs;
pub(crate) mod agent_jobs_spec;
pub(crate) mod apply_patch;
pub(crate) mod apply_patch_retries;
pub(crate) mod apply_patch_spec;
pub(crate) use codex_command_analysis::command_preflight;
pub(crate) use codex_command_analysis::command_search;
pub(crate) use codex_command_analysis::command_shape;
mod checksum_file;
mod compare_artifacts;
mod current_time;
mod dynamic;
pub(crate) mod extension_tools;
mod git_blame;
mod git_inspection;
mod list_available_plugins_to_install;
pub(crate) mod list_available_plugins_to_install_spec;
mod list_files;
mod lookup_executable;
pub(crate) mod mcp;
mod mcp_resource;
pub(crate) mod mcp_resource_spec;
pub(crate) mod multi_agents_common;
pub(crate) mod multi_agents_spec;
pub(crate) mod multi_agents_v2;
mod plan;
pub(crate) mod plan_spec;
mod read_file;
pub(crate) use read_file::syntax_regression;
mod read_tool_output;
pub(crate) mod read_tool_output_spec;
mod request_permissions;
mod request_plugin_install;
pub(crate) mod request_plugin_install_spec;
mod request_user_input;
pub(crate) mod request_user_input_spec;
mod save_turn_diff;
mod shell;
pub(crate) use shell::validation_diagnostic_range;
pub(crate) mod shell_spec;
mod sleep;
mod snapshot_freshness;
mod tool_search;
pub(crate) mod tool_search_spec;
pub(crate) mod unified_exec;
mod view_image;
pub(crate) mod view_image_spec;
mod wait_for_environment;
mod wait_for_file;

use codex_protocol::request_permissions::UriAdditionalPermissionProfile;
use codex_sandboxing::policy_transforms::intersect_uri_permission_profiles;
use codex_sandboxing::policy_transforms::merge_uri_permission_profiles;
use codex_sandboxing::policy_transforms::normalize_additional_permissions;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::AbsolutePathBufGuard;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use crate::FunctionCallError;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::turn_context::TurnEnvironment;
pub(crate) use crate::tools::code_mode::CodeModeExecuteHandler;
pub(crate) use crate::tools::code_mode::CodeModeWaitHandler;
use crate::tools::handlers::command_shape::CommandInvocation;
pub use apply_patch::ApplyPatchHandler;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::AskForApproval;
pub(crate) use checksum_file::ChecksumFileHandler;
pub(crate) use compare_artifacts::CompareArtifactsHandler;
pub use current_time::CurrentTimeHandler;
pub use dynamic::DynamicToolHandler;
pub(crate) use git_blame::GitBlameHandler;
pub(crate) use git_inspection::GitInspectionHandler;
pub use list_available_plugins_to_install::ListAvailablePluginsToInstallHandler;
pub(crate) use list_files::ListFilesHandler;
pub(crate) use lookup_executable::LookupExecutableHandler;
pub use mcp::McpHandler;
pub use mcp_resource::ListMcpResourceTemplatesHandler;
pub use mcp_resource::ListMcpResourcesHandler;
pub use mcp_resource::ReadMcpResourceHandler;
pub use plan::PlanHandler;
pub(crate) use read_file::ReadFileHandler;
pub(crate) use read_file::ReadStatusHandler;
pub(crate) use read_file::compilation_module_paths;
pub(crate) use read_file::canonical_read_file_arguments;
pub(crate) use read_file::existing_tests_changed;

pub(crate) use read_file::validate_read_file_arguments;
pub use read_tool_output::ReadToolOutputHandler;
pub(crate) use read_tool_output::file_only_selector_hint as read_tool_output_file_only_selector_hint;
pub use request_permissions::RequestPermissionsHandler;
pub use request_plugin_install::RequestPluginInstallHandler;
pub use request_user_input::RequestUserInputHandler;
pub(crate) use save_turn_diff::SaveTurnDiffHandler;
pub use shell::ShellCommandHandler;
pub(crate) use shell::ShellCommandHandlerOptions;
pub(crate) use shell::VALIDATION_COMMAND_TIMEOUT_MS;
pub use sleep::SleepHandler;
pub(crate) use snapshot_freshness::CheckSnapshotFreshnessHandler;
pub(crate) use tool_search::ToolSearchHandler;
pub(crate) use tool_search::ToolSearchHandlerCache;
pub use unified_exec::ExecCommandHandler;
pub(crate) use unified_exec::ExecCommandHandlerOptions;
pub use unified_exec::WriteStdinHandler;
pub(crate) use unified_exec::validate_exec_command_arguments;
pub use view_image::ViewImageHandler;
pub(crate) use wait_for_environment::WaitForEnvironmentHandler;
pub(crate) use wait_for_file::WaitForFileHandler;

tokio::task_local! {
    static PARSED_FUNCTION_ARGUMENTS: ParsedFunctionArguments;
}

/// One parsed representation of a function payload for the lifetime of a
/// dispatch. Typed handlers deserialize from this value instead of reparsing
/// the original JSON text.
#[derive(Clone, Debug)]
pub(crate) struct ParsedFunctionArguments {
    raw: Arc<str>,
    value: Result<Arc<Value>, Arc<str>>,
}

impl ParsedFunctionArguments {
    pub(crate) fn from_payload(payload: &crate::tools::context::ToolPayload) -> Option<Self> {
        let crate::tools::context::ToolPayload::Function { arguments } = payload else {
            return None;
        };
        Some(Self::from_raw(arguments))
    }

    fn from_raw(arguments: &str) -> Self {
        // Admission, dispatch and typed handlers can enter nested parse scopes.
        // Reuse only an exact input match; a hook rewrite must parse anew.
        if let Ok(Some(parsed)) = PARSED_FUNCTION_ARGUMENTS
            .try_with(|parsed| (parsed.raw.as_ref() == arguments).then(|| parsed.clone()))
        {
            return parsed;
        }
        Self {
            raw: Arc::from(arguments),
            value: serde_json::from_str(arguments)
                .map(Arc::new)
                .map_err(|err| Arc::from(err.to_string())),
        }
    }

    pub(crate) fn value(&self) -> Result<&Value, &str> {
        self.value.as_deref().map_err(std::convert::AsRef::as_ref)
    }

    fn deserialize<T>(&self, arguments: &str) -> Option<Result<T, FunctionCallError>>
    where
        T: for<'de> Deserialize<'de>,
    {
        if arguments != self.raw.as_ref() {
            return None;
        }
        Some(match &self.value {
            Ok(value) => T::deserialize(value.as_ref()).map_err(|err| {
                invalid_arguments(format!(
                    "failed to parse function arguments: {err}"
                ))
            }),
            Err(message) => Err(invalid_arguments(format!(
                "failed to parse function arguments: {message}"
            ))),
        })
    }

    fn json_value(&self, arguments: &str) -> Option<Result<Value, String>> {
        (arguments == self.raw.as_ref()).then(|| {
            self.value
                .as_ref()
                .map(|value| value.as_ref().clone())
                .map_err(std::string::ToString::to_string)
        })
    }
}

pub(crate) async fn with_parsed_function_arguments<F>(
    parsed: Option<ParsedFunctionArguments>,
    future: F,
) -> F::Output
where
    F: Future,
{
    match parsed {
        Some(parsed) => PARSED_FUNCTION_ARGUMENTS.scope(parsed, future).await,
        None => future.await,
    }
}

pub(crate) fn parsed_function_argument_value(arguments: &str) -> Option<Result<Value, String>> {
    PARSED_FUNCTION_ARGUMENTS
        .try_with(|parsed| parsed.json_value(arguments))
        .ok()
        .flatten()
}

pub(crate) fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    if let Some(parsed) = PARSED_FUNCTION_ARGUMENTS
        .try_with(|parsed| parsed.deserialize(arguments))
        .ok()
        .flatten()
    {
        return parsed;
    }
    serde_json::from_str(arguments).map_err(|err| {
        invalid_arguments(format!("failed to parse function arguments: {err}"))
    })
}

fn invalid_arguments(message: String) -> FunctionCallError {
    FunctionCallError::structured(codex_protocol::tool_failure::ToolFailure::no_effects(
        codex_protocol::tool_failure::ToolFailureCode::InvalidInput,
        message,
    ))
}

fn updated_hook_command(updated_input: &Value) -> Result<&str, FunctionCallError> {
    updated_input
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "hook returned updatedInput without string field `command`".to_string(),
            )
        })
}

fn rewrite_function_arguments(
    arguments: &str,
    tool_name: &str,
    rewrite: impl FnOnce(&mut Map<String, Value>),
) -> Result<String, FunctionCallError> {
    let mut arguments: Value = parse_arguments(arguments)?;
    let Value::Object(arguments) = &mut arguments else {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} arguments must be an object"
        )));
    };
    rewrite(arguments);
    serde_json::to_string(&arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to serialize rewritten {tool_name} arguments: {err}"
        ))
    })
}

fn rewrite_function_command_invocation(
    arguments: &str,
    tool_name: &str,
    field_name: &str,
    command_invocation: &CommandInvocation,
    updated_input: &Value,
) -> Result<String, FunctionCallError> {
    let updated_invocation =
        command_invocation.with_updated_hook_input(tool_name, updated_input)?;
    if &updated_invocation == command_invocation {
        return Ok(arguments.to_string());
    }

    match updated_invocation {
        CommandInvocation::Script(script) => {
            rewrite_function_arguments(arguments, tool_name, |arguments| {
                arguments.insert(field_name.to_string(), Value::String(script));
            })
        }
        CommandInvocation::PowerShellScript(script_body) => {
            rewrite_function_arguments(arguments, tool_name, |arguments| {
                arguments.insert("script_body".to_string(), Value::String(script_body));
            })
        }
        CommandInvocation::Argv { program, args } => {
            rewrite_function_arguments(arguments, tool_name, |arguments| {
                arguments.remove(field_name);
                arguments.remove("script_body");
                arguments.insert("kind".to_string(), Value::String("argv".to_string()));
                arguments.insert("program".to_string(), Value::String(program));
                arguments.insert(
                    "args".to_string(),
                    Value::Array(args.into_iter().map(Value::String).collect()),
                );
            })
        }
    }
}

fn parse_arguments_with_base_path<T>(
    arguments: &str,
    base_path: &AbsolutePathBuf,
) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    let _guard = AbsolutePathBufGuard::new(base_path);
    parse_arguments(arguments)
}

fn resolve_workdir_base_path(
    arguments: &str,
    default_cwd: &AbsolutePathBuf,
) -> Result<AbsolutePathBuf, FunctionCallError> {
    let arguments: Value = parse_arguments(arguments)?;
    Ok(arguments
        .get("workdir")
        .and_then(Value::as_str)
        .filter(|workdir| !workdir.is_empty())
        .map_or_else(|| default_cwd.clone(), |workdir| default_cwd.join(workdir)))
}

pub(crate) fn resolve_tool_environment<'a>(
    environments: &'a TurnEnvironmentSnapshot,
    environment_id: Option<&str>,
) -> Result<Option<&'a TurnEnvironment>, FunctionCallError> {
    environment_id.map_or_else(
        || {
            if environments.starting.iter().any(|environment| environment.selection_index == 0) {
                return Err(FunctionCallError::RespondToModel(
                    "the selected primary environment is not ready; no other environment was selected".into(),
                ));
            }
            Ok(environments.primary())
        },
        |environment_id| {
            environments
                .turn_environments
                .iter()
                .find(|environment| environment.environment_id == environment_id)
                .map(Some)
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(format!(
                        "unknown turn environment id `{environment_id}`"
                    ))
                })
        },
    )
}

/// Read-only tools may wait for the captured environment to finish startup.
pub(crate) async fn wait_for_tool_environment(
    environments: &TurnEnvironmentSnapshot,
    environment_id: Option<&str>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Option<TurnEnvironment>, FunctionCallError> {
    let ready = match environment_id {
        Some(id) => environments
            .turn_environments
            .iter()
            .find(|environment| environment.environment_id == id),
        None => environments.primary(),
    };
    if let Some(ready) = ready {
        let mut ready = ready.clone();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(FunctionCallError::RespondToModel("environment wait cancelled".into())),
            result = ready.refresh_runtime() => result.map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?,
        }
        return Ok(Some(ready));
    }
    let starting = match environment_id {
        Some(id) => environments
            .starting
            .iter()
            .find(|environment| environment.selection.environment_id == id),
        None => environments
            .starting
            .iter()
            .find(|environment| environment.selection_index == 0),
    };
    let Some(starting) = starting else {
        return resolve_tool_environment(environments, environment_id).map(Option::<&_>::cloned);
    };
    let mut ready = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(FunctionCallError::RespondToModel(
            "Cancelled while waiting for execution environment startup; no operation was started.".into())),
        result = starting.wait_until_ready() => result.map_err(|error|
            FunctionCallError::RespondToModel(format!(
                "environment `{}` failed to start: {error}", starting.selection.environment_id))),
    }?;
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(FunctionCallError::RespondToModel("environment wait cancelled".into())),
        result = ready.refresh_runtime() => result.map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?,
    }
    Ok(Some(ready))
}

/// Startup cannot retroactively authorize an action sampled without this scope's
/// instructions. Share the existing cancellable startup wait, but never execute
/// that action; the next sampling refresh discovers the newly ready scope.
pub(crate) async fn wait_for_observed_tool_environment(
    environments: &TurnEnvironmentSnapshot,
    environment_id: Option<&str>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Option<TurnEnvironment>, FunctionCallError> {
    let ready = wait_for_tool_environment(environments, environment_id, cancellation).await?;
    if let Some(environment) = &ready
        && !environments.turn_environments.iter().any(|sampled| {
            sampled.environment_id == environment.environment_id
                && sampled.cwd() == environment.cwd()
                && sampled.owner_id() == environment.owner_id()
        })
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "Environment `{}` became ready after this action was sampled. No operation was started. Its applicable AGENTS.md instructions were not observed for that sampling step. Reconsider the action after the next instruction refresh; do not replay it automatically.",
            environment.environment_id,
        )));
    }
    Ok(ready)
}

/// Whether a fresh inline sandbox override may reach its approval flow.
/// Untrusted mode still classifies ordinary commands itself; it does not accept
/// model-requested sandbox overrides. Preapproved grants bypass this check.
pub(crate) fn allows_inline_sandbox_approval(policy: AskForApproval) -> bool {
    match policy {
        AskForApproval::OnRequest => true,
        AskForApproval::Granular(config) => config.allows_sandbox_approval(),
        AskForApproval::UnlessTrusted | AskForApproval::Never => false,
    }
}

/// Validates feature/policy constraints for `with_additional_permissions` and
/// normalizes any path-based permissions. Errors if the request is invalid.
pub(crate) fn normalize_and_validate_additional_permissions(
    additional_permissions_allowed: bool,
    approval_policy: AskForApproval,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<AdditionalPermissionProfile>,
    permissions_preapproved: bool,
    _cwd: &Path,
) -> Result<Option<AdditionalPermissionProfile>, String> {
    normalize_and_validate_permission_profile(
        additional_permissions_allowed,
        approval_policy,
        sandbox_permissions,
        additional_permissions,
        permissions_preapproved,
        normalize_additional_permissions,
        AdditionalPermissionProfile::is_empty,
    )
}

/// Retains foreign environment paths through permission validation. The native
/// conversion is only a compatibility projection, never the grant authority.
pub(super) fn normalize_and_validate_additional_permissions_uri(
    additional_permissions_allowed: bool,
    approval_policy: AskForApproval,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<UriAdditionalPermissionProfile>,
    permissions_preapproved: bool,
) -> Result<Option<UriAdditionalPermissionProfile>, String> {
    normalize_and_validate_permission_profile(
        additional_permissions_allowed,
        approval_policy,
        sandbox_permissions,
        additional_permissions,
        permissions_preapproved,
        |permissions| {
            match AdditionalPermissionProfile::try_from(permissions.clone()) {
                // Preserve native path normalization for existing local callers.
                Ok(native) => normalize_additional_permissions(native).map(Into::into),
                Err(_) => {
                    codex_sandboxing::policy_transforms::normalize_uri_additional_permissions(
                        permissions,
                    )
                }
            }
        },
        UriAdditionalPermissionProfile::is_empty,
    )
}

fn normalize_and_validate_permission_profile<T>(
    additional_permissions_allowed: bool,
    approval_policy: AskForApproval,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<T>,
    permissions_preapproved: bool,
    normalize: fn(T) -> Result<T, String>,
    is_empty: fn(&T) -> bool,
) -> Result<Option<T>, String> {
    let uses_additional_permissions = matches!(
        sandbox_permissions,
        SandboxPermissions::WithAdditionalPermissions
    );

    if !permissions_preapproved
        && !additional_permissions_allowed
        && (uses_additional_permissions || additional_permissions.is_some())
    {
        return Err(
            "additional permissions are disabled; enable `features.exec_permission_approvals` before using `with_additional_permissions`"
                .to_string(),
        );
    }

    if uses_additional_permissions {
        if !permissions_preapproved && !allows_inline_sandbox_approval(approval_policy) {
            return Err(format!(
                "approval policy is {approval_policy:?}; reject command — this policy does not allow inline sandbox approval requests"
            ));
        }
        let Some(additional_permissions) = additional_permissions else {
            return Err(
                "missing `additional_permissions`; provide at least one of `network` or `file_system` when using `with_additional_permissions`"
                    .to_string(),
            );
        };
        let normalized = normalize(additional_permissions)?;
        if is_empty(&normalized) {
            return Err(
                "`additional_permissions` must include at least one requested permission in `network` or `file_system`"
                    .to_string(),
            );
        }
        return Ok(Some(normalized));
    }

    if additional_permissions.is_some() {
        Err(
            "`additional_permissions` requires `sandbox_permissions` set to `with_additional_permissions`"
                .to_string(),
        )
    } else {
        Ok(None)
    }
}

pub(super) struct EffectiveAdditionalPermissions {
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub additional_permissions_uri: Option<UriAdditionalPermissionProfile>,
    pub permissions_preapproved: bool,
}

pub(super) fn implicit_granted_permissions(
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<&AdditionalPermissionProfile>,
    effective_additional_permissions: &EffectiveAdditionalPermissions,
) -> Option<AdditionalPermissionProfile> {
    if !sandbox_permissions.uses_additional_permissions()
        && !matches!(sandbox_permissions, SandboxPermissions::RequireEscalated)
        && additional_permissions.is_none()
    {
        effective_additional_permissions
            .additional_permissions
            .clone()
    } else {
        None
    }
}

pub(super) async fn apply_granted_turn_permissions(
    session: &Session,
    approval_scope_id: &str,
    cwd: &Path,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<AdditionalPermissionProfile>,
) -> EffectiveAdditionalPermissions {
    let Ok(cwd) = AbsolutePathBuf::from_absolute_path(cwd) else {
        unreachable!("permission matching cwd must be absolute");
    };
    apply_granted_turn_permissions_uri(
        session,
        approval_scope_id,
        &PathUri::from_abs_path(&cwd),
        sandbox_permissions,
        additional_permissions,
    )
    .await
}

pub(super) async fn apply_granted_turn_permissions_uri(
    session: &Session,
    approval_scope_id: &str,
    cwd: &PathUri,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<AdditionalPermissionProfile>,
) -> EffectiveAdditionalPermissions {
    if matches!(sandbox_permissions, SandboxPermissions::RequireEscalated) {
        return EffectiveAdditionalPermissions {
            sandbox_permissions,
            additional_permissions_uri: additional_permissions.clone().map(Into::into),
            additional_permissions,
            permissions_preapproved: false,
        };
    }

    let granted_session_permissions = session.granted_session_permissions(approval_scope_id).await;
    let granted_turn_permissions = session.granted_turn_permissions(approval_scope_id).await;
    let granted_permissions = merge_uri_permission_profiles(
        granted_session_permissions.as_ref(),
        granted_turn_permissions.as_ref(),
    );
    let requested_permissions_uri = additional_permissions.clone().map(Into::into);
    let effective_permissions_uri = merge_uri_permission_profiles(
        requested_permissions_uri.as_ref(),
        granted_permissions.as_ref(),
    );
    let permissions_preapproved = match (effective_permissions_uri.as_ref(), granted_permissions) {
        (Some(effective_permissions), Some(granted_permissions)) => {
            uri_permissions_are_preapproved(effective_permissions, granted_permissions, cwd)
        }
        _ => false,
    };

    let effective_permissions = effective_permissions_uri
        .clone()
        .and_then(|permissions| AdditionalPermissionProfile::try_from(permissions).ok());

    let sandbox_permissions = if effective_permissions_uri.is_some()
        && !sandbox_permissions.uses_additional_permissions()
    {
        SandboxPermissions::WithAdditionalPermissions
    } else {
        sandbox_permissions
    };

    EffectiveAdditionalPermissions {
        sandbox_permissions,
        additional_permissions: effective_permissions,
        additional_permissions_uri: effective_permissions_uri,
        permissions_preapproved,
    }
}

fn uri_permissions_are_preapproved(
    effective_permissions: &UriAdditionalPermissionProfile,
    granted_permissions: UriAdditionalPermissionProfile,
    cwd: &PathUri,
) -> bool {
    use codex_protocol::permissions::FileSystemAccessMode;
    use codex_protocol::permissions::FileSystemPath;

    let mut effective_permissions = effective_permissions.clone();
    let mut granted_permissions = granted_permissions;
    // A deny glob carried by the grant reaches the effective profile unchanged and narrows both
    // sides equally. Compare without it: the intersection rejects every path while the request
    // side holds a glob, so a glob of the request's own still fails the comparison below.
    if let (Some(effective), Some(granted)) = (
        effective_permissions.file_system.as_mut(),
        granted_permissions.file_system.as_mut(),
    ) {
        let shared = granted
            .entries
            .iter()
            .filter(|entry| {
                entry.access == FileSystemAccessMode::Deny
                    && matches!(entry.path, FileSystemPath::GlobPattern { .. })
                    && effective.entries.contains(entry)
            })
            .cloned()
            .collect::<Vec<_>>();
        effective.entries.retain(|entry| !shared.contains(entry));
        granted.entries.retain(|entry| !shared.contains(entry));
    }
    // A stored grant is already bound to the cwd. Bind the effective profile the same way, with
    // its own denies kept off the request side so that they cannot reject its entries here.
    let mut allowed_permissions = effective_permissions.clone();
    if let Some(allowed) = allowed_permissions.file_system.as_mut() {
        allowed
            .entries
            .retain(|entry| entry.access != FileSystemAccessMode::Deny);
    }
    let materialized_effective_permissions =
        intersect_uri_permission_profiles(allowed_permissions, effective_permissions.clone(), cwd);
    intersect_uri_permission_profiles(effective_permissions, granted_permissions, cwd)
        == materialized_effective_permissions
}

