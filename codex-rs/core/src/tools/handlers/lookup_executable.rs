use std::collections::BTreeMap;

use codex_exec_server::LookupExecutableParams;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct LookupExecutableHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    name: String,
    environment_id: Option<String>,
}

impl ToolExecutor<ToolInvocation> for LookupExecutableHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("lookup_executable")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "lookup_executable".into(),
            description: "Resolve a literal executable name or path using the selected Windows execution environment's PATH and PATHEXT, without starting a shell or launching the result. Bare names search PATH directories in order, then PATHEXT suffixes in order. A PATHEXT extension is checked as-is; another dotted name (python3.12) is checked as-is first, then with each PATHEXT suffix. The current directory is searched only when a PATH entry names it (for example `.`) or the name is a path; empty PATH entries are ignored. Relative paths and PATH entries use the selected environment cwd. Missing PATHEXT defaults to .COM;.EXE;.BAT;.CMD; empty PATHEXT stays empty. Uses the executor process environment, not shell profiles, aliases, functions or command-specific env overrides. Reads go through the selected sandboxed filesystem; inaccessible candidates, unresolvable (for example drive-relative) PATH entries, unsupported remote peers and probe-limit failures are errors, not proof of absence. Returns the first matching regular file or found:false. Availability at lookup time does not establish trust, launch permission, successful execution, or future availability. No permission or validation policy is changed.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("name".into(), JsonSchema::string(Some("Literal filename or absolute/cwd-relative path, 1-4096 UTF-8 bytes; no shell quoting or wildcards.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Omit to use the primary execution environment; never falls back to the host.".into()))),
            ]), Some(vec!["name".into()]), Some(false.into())),
            output_schema: Some(json!({
                "type":"object", "additionalProperties":false,
                "properties":{
                    "name":{"type":"string"}, "environment_id":{"type":"string"},
                    "cwd":{"type":"string"}, "found":{"type":"boolean"},
                    "path":{"type":["string","null"]}, "availability_only":{"type":"boolean","const":true}
                },
                "required":["name","environment_id","cwd","found","path","availability_only"]
            }).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "lookup_executable requires function arguments".into(),
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
                    "lookup_executable requires a selected execution environment".into(),
                )
            })?;
            let params = LookupExecutableParams {
                name: args.name.clone(),
                cwd: environment.cwd().clone(),
                sandbox: Some(
                    invocation
                        .step_context
                        .turn
                        .file_system_sandbox_context(None, environment.cwd()),
                ),
            };
            let response = tokio::select! {
                biased;
                _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("executable lookup cancelled".into())),
                result = environment.environment.lookup_executable(params) => result.map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?,
            };
            let output = JsonToolOutput::new(json!({
                "name":args.name, "environment_id":environment.environment_id,
                "cwd":environment.cwd().inferred_native_path_string(), "found":response.path.is_some(),
                "path":response.path.map(|path| path.inferred_native_path_string()), "availability_only":true,
            }));
            crate::tools::context::ensure_exact_model_delivery(&output, &invocation)?;
            Ok(boxed_tool_output(output))
        })
    }
}

impl CoreToolRuntime for LookupExecutableHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
