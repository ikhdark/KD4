use std::collections::BTreeMap;

use codex_exec_server::CapabilityQuery;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use sha2::Digest;

use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct BackendCapabilitiesHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    #[serde(default)]
    operations: Vec<String>,
    environment_id: Option<String>,
    #[serde(default)]
    preflight: bool,
}

/// One identity owner for discovery, startup admission and generation rechecks.
/// The real filesystem operation enforces permissions; the manifest never does.
pub(crate) async fn effective_snapshot(
    environment: &crate::session::turn_context::TurnEnvironment,
    cwd: &codex_utils_path_uri::PathUri,
    sandbox: codex_exec_server::FileSystemSandboxContext,
    configuration: serde_json::Value,
    query: CapabilityQuery,
    validate_workspace: bool,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, FunctionCallError> {
    let work = async {
        if validate_workspace {
            let metadata = environment.environment.get_filesystem()
                .get_metadata(cwd, Some(&sandbox)).await
                .map_err(|error| FunctionCallError::RespondToModel(format!("workspace preflight failed: {error}")))?;
            if !metadata.is_directory {
                return Err(FunctionCallError::RespondToModel("workspace preflight requires an accessible directory".into()));
            }
        }
        let capabilities = environment.environment.capability_report(query).await
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        let configuration = json!({
            "backend_revision": capabilities.manifest.as_ref().map(|manifest| &manifest.revision),
            "environment_id": environment.environment_id,
            "owner_id": environment.owner_id(), "cwd": cwd,
            "sandbox": sandbox, "effective": configuration,
        });
        let configuration_revision = configuration_identity(&configuration)?;
        Ok(json!({
            "environment_id":environment.environment_id,
            "cwd":cwd.inferred_native_path_string(),
            "configuration_revision":configuration_revision,
            "effective_configuration": configuration,
            "workspace_checked": validate_workspace,
            "advisory_only":true, "capabilities":capabilities,
        }))
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(FunctionCallError::RespondToModel("capability preflight cancelled".into())),
        result = work => result,
    }
}

pub(crate) fn configuration_identity(value: &impl serde::Serialize) -> Result<String, FunctionCallError> {
    let mut value = serde_json::to_value(value)
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    // Provider headers/query parameters use HashMaps. Their insertion order is
    // not a configuration change; retain array order and canonicalize objects.
    value.sort_all_objects();
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    Ok(format!("v1:{}", hex::encode(sha2::Sha256::digest(bytes))))
}

pub(crate) fn turn_configuration(turn: &crate::session::turn_context::TurnContext) -> Result<serde_json::Value, FunctionCallError> {
    Ok(json!({
        "model_provider_id": turn.config.model_provider_id,
        "model_provider_revision": configuration_identity(turn.provider.info())?,
        "model": turn.model_info.slug,
        "model_metadata_fallback": turn.model_info.used_fallback_model_metadata,
        "input_modalities": turn.model_info.input_modalities,
        "responses_lite": turn.model_info.use_responses_lite,
        "reasoning_effort": turn.request_reasoning_effort(),
        "service_tier": turn.config.service_tier,
        "developer_instructions_revision": configuration_identity(&turn.developer_instructions)?,
        "base_override_revision": configuration_identity(&turn.config.base_instructions)?,
        "approval_policy": turn.approval_policy.value(),
        "validation_mode": turn.validation_mode,
        "allow_login_shell": turn.config.permissions.allow_login_shell,
        "shell_environment_policy_revision": shell_environment_policy_identity(&turn.config.permissions.shell_environment_policy)?,
        "permission_semantics": "restrictions_only_not_a_grant",
    }))
}

pub(crate) fn shell_environment_policy_identity(
    policy: &codex_protocol::config_types::ShellEnvironmentPolicy,
) -> Result<String, FunctionCallError> {
    // Compiled wildcard patterns are not serializable. Hash their public
    // representation and sorted overrides without exposing environment values.
    configuration_identity(&json!({
        "inherit": policy.inherit,
        "ignore_default_excludes": policy.ignore_default_excludes,
        "exclude": policy.exclude.iter().map(|pattern| (pattern.to_string(), pattern.is_case_insensitive())).collect::<Vec<_>>(),
        "set": policy.r#set.iter().collect::<BTreeMap<_, _>>(),
        "include_only": policy.include_only.iter().map(|pattern| (pattern.to_string(), pattern.is_case_insensitive())).collect::<Vec<_>>(),
        "use_profile": policy.use_profile,
    }))
}

impl ToolExecutor<ToolInvocation> for BackendCapabilitiesHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("backend_capabilities")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "backend_capabilities".into(),
            description: "Refresh versioned, advisory operation support for the selected execution backend and configuration. Optional operations are exact method IDs (for example process/startOwned, fs/readFileRange, fs/mutateFileIfMatches, http/request), not tool names. Returns supported, unsupported or unknown; an incomplete native inventory or legacy peer cannot establish lack of support. Omit operations to enumerate known methods. Backend manifest binds incarnation, version and runtime configuration; configuration_revision additionally binds the selected cwd and sandbox configuration. Support is method-level, not a guarantee for particular arguments, resources or callers. It never grants permission, approves a command, changes policy or replaces execution-time validation. Capabilities may change immediately; execute through the normal owning tool, which revalidates support and permissions. Stale revisions require fresh discovery and reconsideration, never automatic replay. Does not probe by executing operations or fall back to the host.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("operations".into(), JsonSchema::array(JsonSchema::string(None), Some("At most 64 exact operation IDs, each 1-128 UTF-8 bytes without control characters. Empty enumerates known methods.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Omit for the selected primary environment; no host fallback.".into()))),
                ("preflight".into(), JsonSchema::boolean(Some("Also verify workspace directory availability through the selected backend under inherited permissions. Returns effective model/restriction configuration identity. This is an observation, not launch permission; startup and generation recheck sensitive conditions.".into()))),
            ]), None, Some(false.into())),
            output_schema: Some(json!({
                "type":"object",
                "properties":{
                    "environment_id":{"type":"string"}, "cwd":{"type":"string"},
                    "configuration_revision":{"type":"string"}, "advisory_only":{"const":true},
                    "effective_configuration":{"type":"object"}, "workspace_checked":{"type":"boolean"},
                    "capabilities":{"type":"object","properties":{
                        "schemaVersion":{"type":"integer"}, "manifest":{"type":["object","null"]},
                        "inventoryComplete":{"type":"boolean"},
                        "operations":{"type":"object","additionalProperties":{"enum":["supported","unsupported","unknown"]}}
                    },"required":["schemaVersion","manifest","inventoryComplete","operations"]}
                },
                "required":["environment_id","cwd","configuration_revision","advisory_only","capabilities"]
            }).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "backend_capabilities requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            let query = CapabilityQuery {
                methods: args.operations,
            };
            query
                .validate()
                .map_err(|error| FunctionCallError::RespondToModel(error.into()))?;
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
                &invocation.cancellation_token,
            )
            .await?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "backend_capabilities requires a selected execution environment".into(),
                )
            })?;
            let output = JsonToolOutput::new(effective_snapshot(
                &environment, environment.cwd(),
                invocation.step_context.turn.file_system_sandbox_context(None, environment.cwd()),
                turn_configuration(&invocation.step_context.turn)?, query, args.preflight,
                &invocation.cancellation_token,
            ).await?);
            crate::tools::context::ensure_exact_model_delivery(&output, &invocation)?;
            Ok(boxed_tool_output(output))
        })
    }
}

impl CoreToolRuntime for BackendCapabilitiesHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
