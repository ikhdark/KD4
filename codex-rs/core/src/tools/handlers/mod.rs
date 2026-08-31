pub(crate) mod agent_jobs;
pub(crate) mod agent_jobs_spec;
pub(crate) mod apply_patch;
pub(crate) mod apply_patch_spec;
pub(crate) mod command_preflight;
pub(crate) mod command_search;
pub(crate) mod command_shape;
#[cfg(test)]
mod command_windows_corpus_tests;
mod current_time;
mod dynamic;
pub(crate) mod extension_tools;
mod list_available_plugins_to_install;
pub(crate) mod list_available_plugins_to_install_spec;
mod mcp;
mod mcp_resource;
pub(crate) mod mcp_resource_spec;
pub(crate) mod multi_agents;
pub(crate) mod multi_agents_common;
pub(crate) mod multi_agents_spec;
pub(crate) mod multi_agents_v2;
mod plan;
pub(crate) mod plan_spec;
mod read_tool_output;
pub(crate) mod read_tool_output_spec;
mod request_permissions;
mod request_plugin_install;
pub(crate) mod request_plugin_install_spec;
mod request_user_input;
pub(crate) mod request_user_input_spec;
mod shell;
pub(crate) mod shell_spec;
mod sleep;
mod test_sync;
pub(crate) mod test_sync_spec;
mod tool_search;
pub(crate) mod tool_search_spec;
pub(crate) mod unified_exec;
mod view_image;
pub(crate) mod view_image_spec;
mod wait_for_environment;

use codex_git_utils::get_git_repo_root;
use codex_protocol::request_permissions::UriAdditionalPermissionProfile;
#[cfg(test)]
use codex_sandboxing::policy_transforms::intersect_permission_profiles;
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
use std::path::PathBuf;
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
pub use current_time::CurrentTimeHandler;
pub use dynamic::DynamicToolHandler;
pub use list_available_plugins_to_install::ListAvailablePluginsToInstallHandler;
pub use mcp::McpHandler;
pub use mcp_resource::ListMcpResourceTemplatesHandler;
pub use mcp_resource::ListMcpResourcesHandler;
pub use mcp_resource::ReadMcpResourceHandler;
pub use plan::PlanHandler;
pub use read_tool_output::ReadToolOutputHandler;
#[cfg(test)]
pub(crate) use read_tool_output::execute_recovery_transaction;
pub use request_permissions::RequestPermissionsHandler;
pub use request_plugin_install::RequestPluginInstallHandler;
pub use request_user_input::RequestUserInputHandler;
pub use shell::ShellCommandHandler;
pub(crate) use shell::ShellCommandHandlerOptions;
pub use sleep::SleepHandler;
pub use test_sync::TestSyncHandler;
pub(crate) use tool_search::ToolSearchHandlerCache;
pub use unified_exec::ExecCommandHandler;
pub(crate) use unified_exec::ExecCommandHandlerOptions;
pub use unified_exec::WriteStdinHandler;
pub(crate) use unified_exec::validate_exec_command_arguments;
pub use view_image::ViewImageHandler;
pub(crate) use wait_for_environment::WaitForEnvironmentHandler;

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
            Ok(value) => serde_json::from_value(value.as_ref().clone()).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to parse function arguments: {err}"
                ))
            }),
            Err(message) => Err(FunctionCallError::RespondToModel(format!(
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
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
    })
}

pub(crate) fn resolve_repository_root(cwd: &Path) -> PathBuf {
    resolve_repository_root_with(cwd, get_git_repo_root)
}

fn resolve_repository_root_with(
    cwd: &Path,
    discover: impl FnOnce(&Path) -> Option<PathBuf>,
) -> PathBuf {
    discover(cwd).unwrap_or_else(|| cwd.to_path_buf())
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
        || Ok(environments.primary()),
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
        if !permissions_preapproved && !matches!(approval_policy, AskForApproval::OnRequest) {
            return Err(format!(
                "approval policy is {approval_policy:?}; reject command — you cannot request additional permissions unless the approval policy is OnRequest"
            ));
        }
        let Some(additional_permissions) = additional_permissions else {
            return Err(
                "missing `additional_permissions`; provide at least one of `network` or `file_system` when using `with_additional_permissions`"
                    .to_string(),
            );
        };
        let normalized = normalize_additional_permissions(additional_permissions)?;
        if normalized.is_empty() {
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
            additional_permissions,
            additional_permissions_uri: None,
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
    intersect_uri_permission_profiles(effective_permissions.clone(), granted_permissions, cwd)
        == *effective_permissions
}

#[cfg(test)]
fn permissions_are_preapproved(
    effective_permissions: &AdditionalPermissionProfile,
    granted_permissions: AdditionalPermissionProfile,
    cwd: &Path,
) -> bool {
    let materialized_effective_permissions = intersect_permission_profiles(
        effective_permissions.clone(),
        effective_permissions.clone(),
        cwd,
    );
    intersect_permission_profiles(effective_permissions.clone(), granted_permissions, cwd)
        == materialized_effective_permissions
}

#[cfg(test)]
mod tests {
    use super::EffectiveAdditionalPermissions;
    use super::implicit_granted_permissions;
    use super::normalize_and_validate_additional_permissions;
    use super::permissions_are_preapproved;
    use super::resolve_repository_root_with;
    use crate::sandboxing::SandboxPermissions;
    use codex_protocol::models::AdditionalPermissionProfile;
    use codex_protocol::models::FileSystemPermissions;
    use codex_protocol::models::NetworkPermissions;
    use codex_protocol::permissions::FileSystemAccessMode;
    use codex_protocol::permissions::FileSystemPath;
    use codex_protocol::permissions::FileSystemSandboxEntry;
    use codex_protocol::permissions::FileSystemSpecialPath;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::GranularApprovalConfig;
    use codex_sandboxing::policy_transforms::intersect_permission_profiles;
    use codex_sandboxing::policy_transforms::merge_permission_profiles;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[test]
    fn repository_root_resolution_runs_discovery_once() {
        let cwd = std::path::Path::new("workspace/nested");
        let expected = std::path::PathBuf::from("workspace");
        let mut discovery_count = 0;

        let actual = resolve_repository_root_with(cwd, |observed_cwd| {
            discovery_count += 1;
            assert_eq!(observed_cwd, cwd);
            Some(expected.clone())
        });

        assert_eq!(actual, expected);
        assert_eq!(discovery_count, 1);
    }

    fn network_permissions() -> AdditionalPermissionProfile {
        AdditionalPermissionProfile {
            network: Some(NetworkPermissions {
                enabled: Some(true),
            }),
            ..Default::default()
        }
    }

    fn file_system_permissions(path: &std::path::Path) -> AdditionalPermissionProfile {
        AdditionalPermissionProfile {
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                /*read*/ None,
                Some(vec![
                    AbsolutePathBuf::from_absolute_path(path).expect("absolute path"),
                ]),
            )),
            ..Default::default()
        }
    }

    #[test]
    fn preapproved_permissions_work_when_request_permissions_tool_is_enabled_without_exec_permission_approvals_feature()
     {
        let cwd = tempdir().expect("tempdir");

        let normalized = normalize_and_validate_additional_permissions(
            /*additional_permissions_allowed*/ false,
            AskForApproval::Granular(GranularApprovalConfig {
                sandbox_approval: true,
                rules: true,
                skill_approval: true,
                request_permissions: false,
                mcp_elicitations: true,
            }),
            SandboxPermissions::WithAdditionalPermissions,
            Some(network_permissions()),
            /*permissions_preapproved*/ true,
            cwd.path(),
        )
        .expect("preapproved permissions should be allowed");

        assert_eq!(normalized, Some(network_permissions()));
    }

    #[test]
    fn fresh_additional_permissions_still_require_exec_permission_approvals_feature() {
        let cwd = tempdir().expect("tempdir");

        let err = normalize_and_validate_additional_permissions(
            /*additional_permissions_allowed*/ false,
            AskForApproval::OnRequest,
            SandboxPermissions::WithAdditionalPermissions,
            Some(network_permissions()),
            /*permissions_preapproved*/ false,
            cwd.path(),
        )
        .expect_err("fresh inline permission requests should remain disabled");

        assert_eq!(
            err,
            "additional permissions are disabled; enable `features.exec_permission_approvals` before using `with_additional_permissions`"
        );
    }

    #[test]
    fn implicit_sticky_grants_bypass_inline_permission_validation() {
        let cwd = tempdir().expect("tempdir");
        let granted_permissions = file_system_permissions(cwd.path());
        let implicit_permissions = implicit_granted_permissions(
            SandboxPermissions::UseDefault,
            /*additional_permissions*/ None,
            &EffectiveAdditionalPermissions {
                sandbox_permissions: SandboxPermissions::WithAdditionalPermissions,
                additional_permissions: Some(granted_permissions.clone()),
                additional_permissions_uri: None,
                permissions_preapproved: false,
            },
        );

        assert_eq!(implicit_permissions, Some(granted_permissions));
    }

    #[test]
    fn explicit_inline_permissions_do_not_use_implicit_sticky_grant_path() {
        let cwd = tempdir().expect("tempdir");
        let requested_permissions = file_system_permissions(cwd.path());
        let implicit_permissions = implicit_granted_permissions(
            SandboxPermissions::WithAdditionalPermissions,
            Some(&requested_permissions),
            &EffectiveAdditionalPermissions {
                sandbox_permissions: SandboxPermissions::WithAdditionalPermissions,
                additional_permissions: Some(requested_permissions.clone()),
                additional_permissions_uri: None,
                permissions_preapproved: false,
            },
        );

        assert_eq!(implicit_permissions, None);
    }

    #[test]
    fn relative_deny_glob_grants_remain_preapproved_after_materialization() {
        let cwd = tempdir().expect("tempdir");
        let requested_permissions = AdditionalPermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Special {
                            value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                        },
                        access: FileSystemAccessMode::Write,
                    },
                    FileSystemSandboxEntry {
                        path: FileSystemPath::GlobPattern {
                            pattern: "**/*.env".to_string(),
                        },
                        access: FileSystemAccessMode::Deny,
                    },
                ],
                glob_scan_max_depth: None,
            }),
            ..Default::default()
        };
        let stored_grant = intersect_permission_profiles(
            requested_permissions.clone(),
            requested_permissions.clone(),
            cwd.path(),
        );
        let effective_permissions =
            merge_permission_profiles(Some(&requested_permissions), Some(&stored_grant))
                .expect("merged permissions");

        assert!(permissions_are_preapproved(
            &effective_permissions,
            stored_grant,
            cwd.path(),
        ));
    }
}
