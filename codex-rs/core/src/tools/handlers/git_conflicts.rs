use std::collections::BTreeMap;

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

pub(crate) struct GitConflictsHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    #[serde(default)]
    paths: Vec<String>,
    max_files: Option<usize>,
    max_markers: Option<usize>,
    environment_id: Option<String>,
}

impl ToolExecutor<ToolInvocation> for GitConflictsHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("git_conflicts")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        let string = json!({"type":"string"});
        let integer = json!({"type":"integer","minimum":0});
        let stage = json!({"type":"object","additionalProperties":false,"properties":{
            "stage":{"type":"integer","enum":[1,2,3]},"mode":string,"oid":string},"required":["stage","mode","oid"]});
        let marker = json!({"type":"object","additionalProperties":false,"properties":{
            "line":{"type":"integer","minimum":1},"kind":{"type":"string","enum":["start","base","separator","end"]}},"required":["line","kind"]});
        let scan = json!({"type":"object","additionalProperties":false,"properties":{
            "status":{"type":"string","enum":["complete","binary","missing","not_regular_file","unreadable","byte_limit","budget_limit"]},
            "marker_size":{"type":"integer","minimum":1},"bytes_scanned":integer,"markers":{"type":"array","items":marker},
            "markers_truncated":{"type":"boolean"},"error":{"type":["string","null"]}},
            "required":["status","marker_size","bytes_scanned","markers","markers_truncated","error"]});
        let file = json!({"type":"object","additionalProperties":false,"properties":{
            "path":string,"stages":{"type":"array","items":stage},"marker_scan":scan},"required":["path","stages","marker_scan"]});
        ToolSpec::Function(ResponsesApiTool {
            name: "git_conflicts".into(),
            description: "Inventory conflicts from Git's NUL-delimited unmerged index entries, grouped by repository-root-relative path with exact mode/OID and stages (1 base, 2 ours, 3 theirs). Absent stages remain absent, including delete/modify and add/add conflicts. Every returned path is unmerged even when markers are absent: binary files, deleted files and manually edited worktrees may have none. Optional worktree marker hints give 1-based lines and start/base/separator/end kinds, respecting conflict-marker-size (default 7), CRLF and diff3/zdiff3 delimiters. Delimiter-shaped text is a candidate, not proof of an active marker block. NUL-containing data is reported as binary; symlinks/special files are not intentionally followed. Scans are bounded to 8 MiB per returned file and 32 MiB total, with explicit byte/budget/read failures and separate marker-list truncation. total_files covers all selected index conflicts even when files is truncated. Missing markers NEVER imply resolution. Mutable index/worktree observations are not a coherent snapshot. Read-only: never resolves, stages, refreshes or locks the index; no filters, hooks, fetches or repository writes. Local full-disk-read environments only. Index errors, non-UTF-8 paths, timeout and Git capture above 8 MiB fail explicitly, never as no conflicts. No source-read credit or retained recovery handle.".into(),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("paths".into(), JsonSchema { max_items: Some(64), ..JsonSchema::array(JsonSchema::string(None), Some("Literal case-sensitive repository-root-relative files/directories; omitted means all conflicts.".into())) }),
                ("max_files".into(), JsonSchema { minimum: Some(1.into()), maximum: Some(1000.into()), ..JsonSchema::integer(Some("Maximum returned conflicted paths; default 50. Total remains complete.".into())) }),
                ("max_markers".into(), JsonSchema { minimum: Some(1.into()), maximum: Some(1000.into()), ..JsonSchema::integer(Some("Maximum marker locations per returned file; default 20.".into())) }),
                ("environment_id".into(), JsonSchema::string(Some("Selected local execution environment.".into()))),
            ]), Some(vec![]), Some(false.into())),
            output_schema: Some(json!({"type":"object","additionalProperties":false,"properties":{
                "environment_id":string,"repository_root":string,"paths":{"type":"array","items":string},
                "total_files":integer,"files":{"type":"array","items":file},"truncated":{"type":"boolean"},
                "coherent_snapshot_verified":{"type":"boolean"},"warnings":string},
                "required":["environment_id","repository_root","paths","total_files","files","truncated","coherent_snapshot_verified","warnings"]}).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "git_conflicts requires function arguments".into(),
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
                    "git_conflicts requires a selected local environment".into(),
                )
            })?;
            if environment.environment.is_remote()
                || !invocation
                    .step_context
                    .turn
                    .file_system_sandbox_policy()
                    .has_full_disk_read_access()
            {
                return Err(FunctionCallError::RespondToModel("git_conflicts requires a local environment with full disk read access; remote/restricted reads are unsupported".into()));
            }
            crate::tools::parallel::wait_for_workspace_baseline().await;
            let cwd = environment
                .cwd()
                .to_abs_path()
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
                .to_path_buf();
            let result = tokio::select! {
                _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("git_conflicts cancelled; no repository writes were requested".into())),
                result = codex_git_utils::inspect_conflicts(cwd, args.paths, args.max_files.unwrap_or(50), args.max_markers.unwrap_or(20)) => result.map_err(FunctionCallError::RespondToModel)?,
            };
            let mut output = serde_json::to_value(result)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            output["environment_id"] = json!(environment.environment_id);
            let output = JsonToolOutput::new(output);
            crate::tools::context::ensure_exact_model_delivery(&output, &invocation).map_err(|_| {
                FunctionCallError::RespondToModel("Git conflicts exceeds the current model delivery budget; nothing was delivered. Use smaller max_files/max_markers or narrower paths.".into())
            })?;
            Ok(boxed_tool_output(output))
        })
    }
}

impl CoreToolRuntime for GitConflictsHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
