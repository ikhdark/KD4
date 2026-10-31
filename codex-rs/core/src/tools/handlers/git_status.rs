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

pub(crate) struct GitStatusHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    #[serde(default)]
    paths: Vec<String>,
    max_files: Option<usize>,
    environment_id: Option<String>,
    wait_ms: Option<u64>,
    baseline: Option<String>,
    #[serde(default)]
    include_ownership: bool,
}

impl ToolExecutor<ToolInvocation> for GitStatusHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("git_status")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        let string = json!({"type":"string"});
        let entry = json!({"type":"object","additionalProperties":false,"properties":{
            "path":string,"original_path":{"type":["string","null"]},
            "index_status":string,"worktree_status":string},
            "required":["path","original_path","index_status","worktree_status"]});
        let view = json!({"type":"object","additionalProperties":false,"properties":{
            "total_files":{"type":"integer","minimum":0},"files":{"type":"array","items":entry},
            "truncated":{"type":"boolean"}},"required":["total_files","files","truncated"]});
        ToolSpec::Function(ResponsesApiTool {
            name: "git_status".into(),
            description: "Inspect Git's machine-readable NUL-delimited porcelain status with separate staged (HEAD to index), unstaged (index to working tree), untracked and conflicted views. Files with both staged and unstaged edits appear in both views, retaining both exact X/Y status columns (space means unchanged). path is the current/destination repository-root-relative path; original_path preserves the rename/copy source, never a parsed arrow string. Conflicts appear only in conflicted, not as staged resolutions. total_files counts unique status records; view totals overlap and remain complete when their file lists are truncated. Untracked directories are expanded to individual files; ignored files are excluded. Status paths limit changed-file selection, not a tracked-path existence check. Mutable observations are not coherent snapshots. Submodules report checked-out commit changes, not dirty nested working trees. Uses a private index copy, disables configured clean/process filters, hooks and fsmonitor, and never refreshes or locks the caller's index. No fetching or repository writes. Local full-disk-read environments only; failures, non-UTF-8 paths, timeouts and capture above 8 MiB fail explicitly, never as a clean view. Opt in with wait_ms/baseline for deduplicated working-tree/index/ref notifications and with include_ownership for advisory operation/agent lineage; events alone prove no change and hints authorize no overwrite. Use git_diff for patch contents of one comparison. No source-read credit or retained recovery handle.".into(),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("wait_ms".into(), JsonSchema { minimum: Some(0.into()), maximum: Some(300000.into()), ..JsonSchema::integer(Some("Opt in to working-tree notifications: 0 captures a baseline; 1..300000 subscribes to relevant paths plus index/refs BEFORE baseline capture, then rechecks authoritative Git/content state. Duplicate events are ignored. Overflow/degraded coverage returns rescan_required, never proof of a change. Wait budget starts after capture; Git reads retain their own bounded deadlines. No atomic snapshot. Raw capture is bounded to 10000 selected files, 8 MiB/file and 64 MiB total; narrow paths on refusal.".into())) }),
                ("baseline".into(), JsonSchema::string(Some("Optional previous state_sha256 for the same repository/path selection. Compared immediately after subscription; detects changes between calls. Requires wait_ms or include_ownership. Omit to wait for a change from a newly captured baseline.".into()))),
                ("include_ownership".into(), JsonSchema::boolean(Some("Include process-local advisory producing-operation/agent hints for displayed changed paths, preserving overlapping patch lineage. Requires bounded raw capture, even without waiting. Unrecorded/ambiguous changes are unknown. Hints are not locks, exclusive ownership or authority to overwrite; a byte match is not exclusive attribution.".into()))),
                ("paths".into(), JsonSchema { max_items: Some(64), ..JsonSchema::array(JsonSchema::string(None), Some("Literal, case-sensitive repository-root-relative file/directory paths; omitted means the whole repository.".into())) }),
                ("max_files".into(), JsonSchema { minimum: Some(1.into()), maximum: Some(1000.into()), ..JsonSchema::integer(Some("Maximum file records per view; default 50. Totals cover all selected changes.".into())) }),
                ("environment_id".into(), JsonSchema::string(Some("Selected local execution environment.".into()))),
            ]), Some(vec![]), Some(false.into())),
            output_schema: Some(json!({"type":"object","additionalProperties":false,"properties":{
                "environment_id":string,"repository_root":string,"paths":{"type":"array","items":string},
                "total_files":{"type":"integer","minimum":0},"staged":view,"unstaged":view,
                "untracked":view,"conflicted":view,"coherent_snapshot_verified":{"type":"boolean"},"warnings":string,
                "state_sha256":string,"change_detected":{"type":"boolean"},"rescan_required":{"type":"boolean"},"timed_out":{"type":"boolean"},
                "ownership_hints":{"type":"object","additionalProperties":true}},
                "required":["environment_id","repository_root","paths","total_files","staged","unstaged","untracked","conflicted","coherent_snapshot_verified","warnings"]}).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "git_status requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            if args.wait_ms.is_some_and(|wait| wait > 300000)
                || (args.baseline.is_some() && args.wait_ms.is_none() && !args.include_ownership)
            {
                return Err(FunctionCallError::RespondToModel("wait_ms must be 0..300000; baseline requires wait_ms or include_ownership".into()));
            }
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
                &invocation.cancellation_token,
            )
            .await?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "git_status requires a selected local environment".into(),
                )
            })?;
            if environment.environment.is_remote()
                || !invocation
                    .step_context
                    .turn
                    .file_system_sandbox_policy()
                    .has_full_disk_read_access()
            {
                return Err(FunctionCallError::RespondToModel("git_status requires a local environment with full disk read access; remote/restricted reads are unsupported".into()));
            }
            crate::tools::parallel::wait_for_workspace_baseline().await;
            let cwd = environment
                .cwd()
                .to_abs_path()
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
                .to_path_buf();
            let result = tokio::select! {
                _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("git_status cancelled; no repository writes were requested".into())),
                result = async {
                    if args.wait_ms.is_some() || args.include_ownership {
                        let observed = codex_git_utils::observe_working_tree(cwd, args.paths, args.max_files.unwrap_or(50), args.baseline,
                            std::time::Duration::from_millis(args.wait_ms.unwrap_or(0))).await?;
                        let hints = if args.include_ownership {
                            let reported = [&observed.status.staged, &observed.status.unstaged, &observed.status.untracked, &observed.status.conflicted]
                                .into_iter().flat_map(|view| &view.files)
                                .flat_map(|entry| std::iter::once(entry.path.as_str()).chain(entry.original_path.as_deref()))
                                .map(|path| observed.status.repository_root.join(path)).collect::<std::collections::BTreeSet<_>>();
                            let files = observed.files.iter().filter(|file| reported.contains(&file.path))
                                .map(|file| (file.path.clone(), file.sha256.clone())).collect::<Vec<_>>();
                            Some(codex_apply_patch::change_ownership_hints(&files))
                        } else { None };
                        let mut value = serde_json::to_value(observed).map_err(|error| error.to_string())?;
                        if let Some(hints) = hints { value["ownership_hints"] = hints; }
                        Ok::<_, String>(value)
                    } else {
                        serde_json::to_value(codex_git_utils::inspect_status(cwd, args.paths, args.max_files.unwrap_or(50)).await?)
                            .map_err(|error| error.to_string())
                    }
                } => result.map_err(FunctionCallError::RespondToModel)?,
            };
            let mut output = result;
            output["environment_id"] = json!(environment.environment_id);
            let output = JsonToolOutput::new(output);
            crate::tools::context::ensure_exact_model_delivery(&output, &invocation).map_err(|_| {
                FunctionCallError::RespondToModel("Git status exceeds the current model delivery budget; nothing was delivered. Use smaller max_files or narrower paths.".into())
            })?;
            Ok(boxed_tool_output(output))
        })
    }
}

impl CoreToolRuntime for GitStatusHandler {
    fn waits_for_workspace_changes(&self, payload: &ToolPayload) -> bool {
        let ToolPayload::Function { arguments } = payload else { return false; };
        serde_json::from_str::<Arguments>(arguments).ok()
            .is_some_and(|args| args.wait_ms.is_some_and(|wait| (1..=300000).contains(&wait)))
    }

    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
