use std::collections::BTreeMap;

use codex_git_utils::BlameSource;
use codex_git_utils::MAX_BLAME_LINES;
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

pub(crate) struct GitBlameHandler;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Revision,
    WorkingTree,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    path: String,
    start: usize,
    end: usize,
    revision: Option<String>,
    source: Option<Source>,
    environment_id: Option<String>,
}

impl ToolExecutor<ToolInvocation> for GitBlameHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("git_blame")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        let range = json!({"type":"object","additionalProperties":false,
            "properties":{"start":{"type":"integer","minimum":1},"end":{"type":"integer","minimum":1}},
            "required":["start","end"]});
        let nullable_string = json!({"type":["string","null"]});
        ToolSpec::Function(ResponsesApiTool {
            name: "git_blame".into(),
            description: "Secondary Git attribution for one repository-root-relative regular UTF-8 file and a one-based inclusive line range (maximum 200 lines; file limit 8 MiB). revision defaults to HEAD and resolves once to an immutable commit. source:revision (default) blames that committed blob, ignoring working-tree edits; source:working_tree captures current bytes and blames them against that resolved commit with Git's built-in EOL normalization (as git_diff), following index-tracked renames and marking changed/new lines working_tree with no commit attribution. Working-tree attribution is explicitly unavailable for files with filter/working-tree-encoding/ident attributes or a path .gitattributes present in the index but missing or unreadable in the working tree, during a merge, or for untracked paths that are not exactly a file of this repository (case, symlinked directory, submodule). blob_oid identifies the blamed content (after EOL normalization); working-tree blobs are not written to Git and may not remain current. Groups bind final ranges to original paths/line starts and commit IDs; zero-OID pseudo-commits are never committed authorship. Missing repositories/revisions/objects/files or exhausted history are explicit unavailable_reason, not clean/no-change results. Boundary commits with unavailable parents are unavailable_history, not definitive origins; true roots remain committed. range_complete measures requested-line coverage; history_complete is separate. No fetching, filters/textconv, hooks, writes to the repository or author-intent inference. Local environments with full disk read access only. This does not read source text for review coverage: use read_file (with the returned commit_oid for committed source) and prioritize missing source reads over blame.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("path".into(), JsonSchema::string(Some("Repository-root-relative path; no traversal or absolute paths.".into()))),
                ("start".into(), JsonSchema { minimum: Some(1.into()), ..JsonSchema::integer(Some("First line, inclusive, one-based.".into())) }),
                ("end".into(), JsonSchema { minimum: Some(1.into()), ..JsonSchema::integer(Some("Last line, inclusive; at most 200 requested lines.".into())) }),
                ("revision".into(), JsonSchema::string(Some("Commit revision resolved once; default HEAD, including the working-tree baseline.".into()))),
                ("source".into(), JsonSchema { enum_values: Some(vec!["revision".into(), "working_tree".into()]), ..JsonSchema::string(Some("Default revision; working_tree explicitly opts into captured uncommitted bytes.".into())) }),
                ("environment_id".into(), JsonSchema::string(Some("Selected local execution environment.".into()))),
            ]), Some(vec!["path".into(), "start".into(), "end".into()]), Some(false.into())),
            output_schema: Some(json!({"type":"object","additionalProperties":false,"properties":{
                "environment_id":{"type":"string"},"repository_root":nullable_string,"path":{"type":"string"},"revision":{"type":"string"},
                "source":{"type":"string","enum":["revision","working_tree"]},
                "commit_oid":nullable_string,"blob_oid":nullable_string,"range":range,
                "range_complete":{"type":"boolean"},"history_complete":{"type":"boolean"},
                "unavailable_reason":nullable_string,
                "groups":{"type":"array","items":{"type":"object","additionalProperties":false,"properties":{
                    "range":range,"attribution":{"type":"string","enum":["committed","working_tree","unavailable_history"]},
                    "commit_oid":nullable_string,"original_path":nullable_string,
                    "original_start":{"type":["integer","null"],"minimum":1},"boundary":{"type":"boolean"}
                },"required":["range","attribution","commit_oid","original_start","original_path","boundary"]}}
            },"required":["environment_id","repository_root","path","revision","source","commit_oid","blob_oid","range","range_complete","history_complete","groups","unavailable_reason"]}).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "git_blame requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            if args.start == 0 || args.end < args.start || args.end - args.start >= MAX_BLAME_LINES
            {
                return Err(FunctionCallError::RespondToModel(format!(
                    "git_blame requires 1-{MAX_BLAME_LINES} lines in a one-based inclusive range"
                )));
            }
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
                &invocation.cancellation_token,
            )
            .await?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "git_blame requires a selected local environment".into(),
                )
            })?;
            if environment.environment.is_remote()
                || !invocation
                    .step_context
                    .turn
                    .file_system_sandbox_policy()
                    .has_full_disk_read_access()
            {
                return Err(FunctionCallError::RespondToModel("git_blame requires a local environment with full disk read access; remote/restricted Git reads are not supported".into()));
            }
            crate::tools::parallel::wait_for_workspace_baseline().await;
            let cwd = environment
                .cwd()
                .to_abs_path()
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let source = match args.source.unwrap_or(Source::Revision) {
                Source::Revision => BlameSource::Revision,
                Source::WorkingTree => BlameSource::WorkingTree,
            };
            let result = tokio::select! {
                _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("git_blame cancelled".into())),
                result = codex_git_utils::blame_range(cwd.to_path_buf(), args.path,
                    args.revision.unwrap_or_else(|| "HEAD".into()), source, args.start, args.end) => result.map_err(FunctionCallError::RespondToModel)?,
            };
            let mut output = serde_json::to_value(result)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            output["environment_id"] = json!(environment.environment_id);
            let output = JsonToolOutput::new(output);
            crate::tools::context::ensure_exact_model_delivery(&output, &invocation).map_err(
                |_| {
                    FunctionCallError::RespondToModel("git_blame result exceeds the current model delivery budget; nothing was delivered. Request a smaller line range.".into())
                },
            )?;
            Ok(boxed_tool_output(output))
        })
    }
}

impl CoreToolRuntime for GitBlameHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
