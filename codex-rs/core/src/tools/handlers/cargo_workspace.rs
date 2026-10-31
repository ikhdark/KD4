use std::collections::BTreeMap;

use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_path_uri::PathConvention;
use serde::Deserialize;
use serde_json::json;

use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct CargoWorkspaceHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    manifest_path: Option<String>,
    workdir: Option<String>,
    environment_id: Option<String>,
    offline: Option<bool>,
    locked: Option<bool>,
    max_output_tokens: Option<usize>,
}

impl ToolExecutor<ToolInvocation> for CargoWorkspaceHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("cargo_workspace")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "cargo_workspace".into(),
            description: "Discover Cargo workspace packages and targets with Cargo's own JSON metadata in the selected Windows execution environment. Runs the selected PATH Cargo's --version, locate-project --message-format=json, then metadata --format-version=1 --no-deps using that exact manifest and executable. Does not reconstruct workspace membership, target inference or dependency resolution. offline and locked default true; false only omits that flag and never overrides Cargo configuration/environment requirements. Returns the normal exec_command receipt: stdout contains one JSON provenance record (Cargo executable/version, selected manifest, requested flags), followed by Cargo's unchanged metadata JSON. Cargo failures retain stderr and nonzero exits, never substitute inventories; a provenance record alone is not a successful inventory. Large output retains the normal command recovery route. If still running, resume the returned process with write_stdin; do not launch another discovery. Uses the existing exec_command authorization, hooks, sandbox, validation, cancellation and process ownership; requires that execution tool and a PowerShell-capable Windows environment. No host fallback, builds or tests. Cargo/rustup retain their ordinary cache and toolchain behavior under the execution policy; discovery grants no provisioning authority and is not a read-only filesystem operation.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("manifest_path".into(), JsonSchema::string(Some("Optional literal Cargo manifest path, relative to workdir. Omitted: Cargo selects it from workdir.".into()))),
                ("workdir".into(), JsonSchema::string(Some("Execution cwd; omitted uses the selected environment cwd.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Omit for the primary execution environment; never falls back to the host.".into()))),
                ("offline".into(), JsonSchema::boolean(Some("Pass --offline; default true. Does not relax configured offline requirements.".into()))),
                ("locked".into(), JsonSchema::boolean(Some("Pass --locked; default true. Does not relax configured locked requirements.".into()))),
                ("max_output_tokens".into(), JsonSchema::integer(Some("Command display budget; original output uses normal exec_command retention/recovery.".into()))),
            ]), Some(vec![]), Some(false.into())),
            output_schema: Some(super::shell_spec::unified_exec_eager_output_schema().into()),
        })
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Err(FunctionCallError::RespondToModel(
                "cargo_workspace must be resolved through the registered exec_command runtime"
                    .into(),
            ))
        })
    }
}

impl CoreToolRuntime for CargoWorkspaceHandler {
    fn prepare_invocation<'a>(
        &'a self,
        mut invocation: ToolInvocation,
    ) -> futures::future::BoxFuture<'a, Result<ToolInvocation, FunctionCallError>> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "cargo_workspace requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
                &invocation.cancellation_token,
            )
            .await?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "cargo_workspace requires a selected execution environment".into(),
                )
            })?;
            if environment.cwd().infer_path_convention() != Some(PathConvention::Windows) {
                return Err(FunctionCallError::RespondToModel("cargo_workspace requires a Windows execution environment; no host fallback was attempted".into()));
            }
            let manifest = match args.manifest_path {
                Some(path) if path.is_empty() || path.contains('\0') => {
                    return Err(FunctionCallError::RespondToModel(
                        "manifest_path must be a nonempty literal path without NUL".into(),
                    ));
                }
                Some(path) => format!("'--manifest-path={}'", path.replace('\'', "''")),
                None => String::new(),
            };
            let offline = args.offline.unwrap_or(true);
            let locked = args.locked.unwrap_or(true);
            let script = format!(
                "$manifestArgs = @({manifest})\n$metadataArgs = @('--format-version=1', '--no-deps'{}{})\n{}",
                if offline { ", '--offline'" } else { "" },
                if locked { ", '--locked'" } else { "" },
                include_str!("cargo_workspace.ps1"),
            );
            // Resolve before hooks/admission so the existing execution owner sees
            // the actual script, selected environment and complete policy surface.
            invocation.tool_name = ToolName::plain("exec_command");
            invocation.payload = ToolPayload::Function {
                arguments: json!({
                    "script_body":script, "login":false,
                    "environment_id":environment.environment_id,
                    "workdir":args.workdir, "yield_time_ms":300000,
                    "max_output_tokens":args.max_output_tokens,
                })
                .to_string(),
            };
            Ok(invocation)
        })
    }
}
