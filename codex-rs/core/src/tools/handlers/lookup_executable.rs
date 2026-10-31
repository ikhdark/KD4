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
    name: Option<String>,
    names: Option<Vec<String>>,
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
            description: "Resolve a literal executable name/path (`name`) or 1-32 independent names/paths (`names`, mutually exclusive) using the selected Windows execution environment's PATH and PATHEXT, without starting a shell or launching the result. Bare names search PATH directories in order, then PATHEXT suffixes in order. A PATHEXT extension is checked as-is; another dotted name (python3.12) is checked as-is first, then with each PATHEXT suffix. The current directory is searched only when a PATH entry names it (for example `.`) or the name is a path; empty PATH entries are ignored. Relative paths and PATH entries use the selected environment cwd. Missing PATHEXT defaults to .COM;.EXE;.BAT;.CMD; empty PATHEXT stays empty. Uses the executor process environment, not shell profiles, aliases, functions or command-specific env overrides. Reads go through the selected sandboxed filesystem; inaccessible candidates, unresolvable (for example drive-relative) PATH entries, unsupported remote peers and probe-limit failures are errors, not proof of absence. Returns the first matching regular file or found:false. Availability at lookup time does not establish trust, launch permission, successful execution, or future availability. No permission or validation policy is changed.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("name".into(), JsonSchema::string(Some("Single literal filename or absolute/cwd-relative path, 1-4096 UTF-8 bytes; mutually exclusive with names.".into()))),
                ("names".into(), JsonSchema { min_items: Some(1), max_items: Some(32), ..JsonSchema::array(JsonSchema::string(None), Some("1-32 literal executable names/paths. Ordered independent results, including invalid or duplicate names; mutually exclusive with name. Missing is distinct from inaccessible/unknown. At most 16384 candidate probes and 4 MiB of candidate paths per batch; exhausted lookups are unknown, not missing.".into())) }),
                ("environment_id".into(), JsonSchema::string(Some("Omit to use the primary execution environment; never falls back to the host.".into()))),
            ]), None, Some(false.into())),
            output_schema: Some(json!({
                "oneOf":[{
                "type":"object", "additionalProperties":false,
                "properties":{
                    "name":{"type":"string"}, "environment_id":{"type":"string"},
                    "cwd":{"type":"string"}, "found":{"type":"boolean"},
                    "path":{"type":["string","null"]}, "availability_only":{"type":"boolean","const":true}
                },
                "required":["name","environment_id","cwd","found","path","availability_only"]
                },{
                    "type":"object", "additionalProperties":false,
                    "properties":{
                        "environment_id":{"type":"string"}, "cwd":{"type":"string"},
                        "availability_only":{"const":true},
                        "results":{"type":"array","minItems":1,"maxItems":32,"items":{
                            "type":"object","additionalProperties":false,
                            "properties":{
                                "name":{"type":"string"}, "found":{"type":["boolean","null"]},
                                "path":{"type":["string","null"]}, "error":{"type":["string","null"]},
                                "status":{"enum":["found","missing","inaccessible","unsupported","invalid","unknown"]}
                            }, "required":["name","found","path","status","error"]
                        }}
                    }, "required":["environment_id","cwd","availability_only","results"]
                }]
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
            if args.name.is_some() == args.names.is_some()
                || args.names.as_ref().is_some_and(|names| names.is_empty() || names.len() > 32)
            {
                return Err(FunctionCallError::RespondToModel("provide name or names (1-32 entries), exclusively".into()));
            }
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
            if let Some(names) = args.names {
                let params = codex_exec_server::LookupExecutablesParams {
                    names: names.clone(), cwd: environment.cwd().clone(),
                    sandbox: Some(invocation.step_context.turn.file_system_sandbox_context(None, environment.cwd())),
                };
                let results = tokio::select! {
                    biased;
                    _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("executable lookup cancelled".into())),
                    result = environment.environment.lookup_executables(params) => result.map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?,
                };
                if results.len() != names.len() {
                    return Err(FunctionCallError::RespondToModel("executable batch result count mismatch".into()));
                }
                let results = names.into_iter().zip(results).map(|(name, result)| {
                    use codex_file_system::FileSystemLookupResult as R;
                    let (found, path, status, error) = match result {
                        R::Resolved { path } => (Some(true), Some(path.inferred_native_path_string()), "found", None),
                        R::Missing => (Some(false), None, "missing", None),
                        R::Inaccessible { error } => (None, None, "inaccessible", Some(error)),
                        R::Unsupported { error } => (None, None, "unsupported", Some(error)),
                        R::Invalid { error } => (None, None, "invalid", Some(error)),
                        R::Unknown { error } => (None, None, "unknown", Some(error)),
                    };
                    json!({"name":name,"found":found,"path":path,"status":status,"error":error})
                }).collect::<Vec<_>>();
                let output = JsonToolOutput::new(json!({"environment_id":environment.environment_id,
                    "cwd":environment.cwd().inferred_native_path_string(),"availability_only":true,"results":results}));
                crate::tools::context::ensure_exact_model_delivery(&output, &invocation)?;
                return Ok(boxed_tool_output(output));
            }
            let name = args.name.expect("exclusive name validated above");
            let params = LookupExecutableParams {
                name: name.clone(),
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
                "name":name, "environment_id":environment.environment_id,
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
