use std::collections::BTreeMap;

use codex_exec_server::WalkFilters;
use codex_exec_server::WalkOptions;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathConvention;
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

pub(crate) struct ListFilesHandler;

const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 50_000;

fn render_listing_path(cwd: &str, native: String, convention: Option<PathConvention>) -> String {
    let Some(convention) = convention else {
        return native;
    };
    let windows = convention == PathConvention::Windows;
    let normalized_cwd = windows.then(|| cwd.replace('\\', "/"));
    let cwd = normalized_cwd.as_deref().unwrap_or(cwd);
    let normalized = windows.then(|| native.replace('\\', "/"));
    let path = normalized.as_deref().unwrap_or(&native);
    match path.strip_prefix(cwd) {
        Some("") => ".".to_string(),
        Some(rest) if cwd.ends_with('/') => rest.to_string(),
        Some(rest) if rest.starts_with('/') => rest[1..].to_string(),
        _ => native,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListFilesArgs {
    path: String,
    environment_id: Option<String>,
    max_depth: Option<usize>,
    max_entries: Option<usize>,
    max_directories: Option<usize>,
    include_hidden: Option<bool>,
    include_ignored: Option<bool>,
    include_metadata: Option<bool>,
    summary_only: Option<bool>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    exclude_directories: Vec<String>,
    #[serde(default)]
    include_paths: Vec<String>,
    #[serde(default)]
    exclude_paths: Vec<String>,
    #[serde(default)]
    exclude_directory_paths: Vec<String>,
}

impl ToolExecutor<ToolInvocation> for ListFilesHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("list_files")
    }

    fn spec(&self) -> ToolSpec {
        let mut depth = JsonSchema::integer(Some(
            "Maximum descendant depth, 0 for an immediate directory listing; default 8, clamped to 64 (returned in effective_walk_options).".into(),
        ));
        depth.minimum = Some(0.into());
        let mut entries = JsonSchema::integer(Some(
            "Maximum entries examined, including directories; with include patterns, maximum matching files returned instead while up to 50000 entries are examined. Default 2000, clamped to 50000 (returned in effective_walk_options).".into(),
        ));
        entries.minimum = Some(1.into());
        ToolSpec::Function(ResponsesApiTool {
            name: "list_files".into(),
            description: "List files and directories through the selected environment's sandboxed filesystem without shell quoting. Start at a narrow path. For the same root and walk options, combine independently known include patterns in one call instead of repeating the walk; limits apply to the combined results, so check completeness. File include/exclude filters do not prune directories; with include patterns, entries are only matching files and directories are traversed without being listed. include/exclude/exclude_directories retain case-sensitive basename matching at every depth: * matches zero or more Unicode characters, ? one; other characters are literal and separators are forbidden. Explicit include_paths/exclude_paths/exclude_directory_paths match case-sensitive paths relative to the selected root, not cwd. Both / and \\ are pattern separators; * and ? cannot cross them, and whole-segment ** matches zero or more segments. Other characters are literal; no escaping, braces or character classes. Inclusion groups are unioned; either exclusion group wins. Only exclude_directories and exclude_directory_paths omit and prune directories; the selected root is never excluded. At most 64 patterns across all six groups, each 1–256 UTF-8 bytes without NUL. Path patterns forbid absolute/drive paths, colon, empty/dot/parent segments and embedded **; trailing or repeated separators are invalid. Entry paths are relative to the environment cwd (absolute when outside it) and can be passed to read_file or list_files. Directory symlinks are omitted and never followed. File symlinks (including dangling ones) describe the link itself, not its target. Optional include_metadata adds cheap enumeration metadata only, without content/MIME detection or extra per-entry probes; hidden directories are listed (except with include patterns) but not traversed unless include_hidden is true. By default, as in Git and ripgrep, .gitignore files apply only inside Git repositories: each entry is matched against those from its nearest enclosing repository root (a directory containing .git) down, and entries outside any repository are never gitignored. Ignored files and directories are omitted and pruned (gitignored_omitted counts them; a pruned directory counts once). The selected path is listed even inside an ignored directory. Set include_ignored=true to bypass these rules, independently of include_hidden. Global Git excludes, .git/info/exclude, .ignore and .rgignore are not applied. For an unfiltered audit omit all filters and set include_hidden=true and include_ignored=true. Ignore-file read, size or parse failures are explicit errors, never silently complete listings. complete refers only to this requested scope. Check truncated, errors, unexplored cutoff reasons, and effective_walk_options before claiming coverage. Directories are mutable, not stable pages. Recover large output with read_tool_output; repeat list_files for current state.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("path".into(), JsonSchema::string(Some("Directory path, relative to the environment cwd or absolute.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Omit to use the primary environment.".into()))),
                ("max_depth".into(), depth),
                ("max_entries".into(), entries),
                ("max_directories".into(), JsonSchema { minimum: Some(1.into()), ..JsonSchema::integer(Some("Maximum directories traversed, including the root. Default min(max_entries,10000), or 10000 with include patterns; clamped to 10000.".into())) }),
                ("include_hidden".into(), JsonSchema::boolean(Some("Traverse hidden directories; default false.".into()))),
                ("include_ignored".into(), JsonSchema::boolean(Some("Include and traverse gitignored files/directories; default false. Independent of include_hidden, which must be set separately for hidden-directory traversal.".into()))),
                ("summary_only".into(), JsonSchema::boolean(Some("Aggregate during the existing scan instead of returning entries; default false. Returns summary with file/directory counts, logical file_bytes, scan counts, skipped counts by reason, and totals_complete. The selected root is excluded; directories count eligible discovered directories even with file includes. Bytes use enumeration metadata, counted per path (file links use link size; directory links are omitted and never followed). Filters, hidden/ignored policy and all scan limits still apply. Skipped counts cover observed entries/subtrees, not unknown descendants. Inaccessible entries have path errors; cutoffs and errors make totals partial. No content reads or second traversal; metadata can still require filesystem calls. Totals describe this requested scope at scan time, not a stable or whole-disk snapshot.".into()))),
                ("include_metadata".into(), JsonSchema::boolean(Some("Include per-entry metadata; default false (omitted from entries). Fields: size in bytes, created_at_ms and modified_at_ms (Unix milliseconds, null if unavailable), readonly, is_symlink. Values describe the enumerated entry, never a file-link target; directory links are omitted. No content reads, MIME detection or additional per-entry metadata calls. Failures retain path/message in errors; addressable classified entries without details have metadata_error and complete=false. summary_only returns no entries even when this option is true.".into()))),
                ("include".into(), JsonSchema::array(JsonSchema::string(None), Some("File basename patterns, unioned with include_paths. If both are empty or omitted, includes all files; otherwise only matching files are listed. Directories are still traversed.".into()))),
                ("exclude".into(), JsonSchema::array(JsonSchema::string(None), Some("File basename patterns omitted from results; exclusion wins over inclusion.".into()))),
                ("exclude_directories".into(), JsonSchema::array(JsonSchema::string(None), Some("Directory basename patterns omitted and never traversed, e.g. [\"node_modules\",\"target\"]. The explicitly selected root is never excluded.".into()))),
                ("include_paths".into(), JsonSchema::array(JsonSchema::string(None), Some("Case-sensitive file paths relative to the selected path, not cwd; unioned with include. Use / or \\ separators; * and ? match within a segment, whole-segment ** matches zero or more segments. Other characters are literal. Never prunes directories, e.g. [\"src/**/*.rs\"].".into()))),
                ("exclude_paths".into(), JsonSchema::array(JsonSchema::string(None), Some("Root-relative file path patterns using include_paths syntax. Either exclude or exclude_paths wins over either inclusion group. Never prunes directories.".into()))),
                ("exclude_directory_paths".into(), JsonSchema::array(JsonSchema::string(None), Some("Root-relative directory path patterns using include_paths syntax; matching directories are omitted and never traversed, e.g. [\"src/generated\"]. The selected root is never excluded.".into()))),
            ]), Some(vec!["path".into()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "list_files requires function arguments".into(),
                ));
            };
            let args: ListFilesArgs = parse_arguments(arguments)?;
            let max_depth = args.max_depth.unwrap_or(8).min(MAX_DEPTH);
            let max_entries = args.max_entries.unwrap_or(2000).min(MAX_ENTRIES);
            // Unfiltered walks list every traversed directory as an entry. Include
            // patterns list only matching files, so entries do not bound traversal.
            let max_directories = args
                .max_directories
                .unwrap_or(if args.include.is_empty() && args.include_paths.is_empty() {
                    max_entries
                } else {
                    10_000
                })
                .min(10_000);
            let filters = WalkFilters {
                include: args.include,
                exclude: args.exclude,
                exclude_directories: args.exclude_directories,
                include_paths: args.include_paths,
                exclude_paths: args.exclude_paths,
                exclude_directory_paths: args.exclude_directory_paths,
            };
            filters
                .validate()
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            if max_entries == 0 || max_directories == 0 {
                return Err(FunctionCallError::RespondToModel(format!(
                    "list_files requires max_depth 0-{MAX_DEPTH}, max_entries 1-{MAX_ENTRIES}, and max_directories 1-10000"
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
                    "list_files requires a selected execution environment".into(),
                )
            })?;
            let path = environment
                .cwd()
                .join(&args.path)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let sandbox = invocation
                .step_context
                .turn
                .file_system_sandbox_context(None, environment.cwd());
            let filesystem = environment.environment.get_filesystem();
            let options = WalkOptions {
                max_depth,
                max_directories,
                max_entries,
                follow_directory_symlinks: false,
                summary_only: args.summary_only.unwrap_or(false),
                include_metadata: args.include_metadata.unwrap_or(false),
                prune_hidden_directories: !args.include_hidden.unwrap_or(false),
                apply_gitignore: !args.include_ignored.unwrap_or(false),
                filters,
            };
            let outcome = tokio::select! {
                biased;
                _ = invocation.cancellation_token.cancelled() => {
                    return Err(FunctionCallError::RespondToModel("list_files cancelled".into()));
                }
                result = filesystem.walk(&path, options.clone(), Some(&sandbox)) => result,
            };
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    let mut message = format!("unable to list {}: {error}", path.inferred_native_path_string());
                    if error.kind() == std::io::ErrorKind::NotFound
                        && !environment.environment.is_remote()
                        && !invocation.cancellation_token.is_cancelled()
                        && invocation.step_context.turn.file_system_sandbox_policy().has_full_disk_read_access()
                        && let (Ok(missing), Ok(cwd)) = (path.to_abs_path(), environment.cwd().to_abs_path())
                    {
                        let suggestions = crate::tools::run_blocking_command_analysis(move || {
                            super::command_search::nearest_existing_paths(missing.as_path(), cwd.as_path())
                        }).await.unwrap_or_default();
                        if !suggestions.is_empty() {
                            message.push_str("\nPossible existing paths (not verified replacements): ");
                            message.push_str(&suggestions.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", "));
                        }
                    }
                    return Err(FunctionCallError::RespondToModel(message));
                }
            };
            // read_file and list_files take native paths, not URIs. The
            // cwd-relative spelling also keeps every entry from repeating the root.
            let cwd = environment.cwd().inferred_native_path_string();
            let convention = environment.cwd().infer_path_convention();
            let display_path = |native| render_listing_path(&cwd, native, convention);
            let mut errors = outcome.errors.iter().map(|error| json!({
                "path": display_path(error.path.inferred_native_path_string()), "message": error.message,
            })).collect::<Vec<_>>();
            let entries = outcome
                .entries
                .iter()
                .filter_map(|entry| {
                    // The filesystem can preserve opaque native identities, but this
                    // tool publishes strings accepted by read_file/list_files. A lossy
                    // replacement label must never name a different healthy sibling.
                    let native = entry.path.infer_path_convention().and_then(|convention| {
                        LegacyAppPathString::from_path_uri(&entry.path, convention)
                            .ok()
                            .filter(|native| {
                                native
                                    .to_path_uri(convention)
                                    .is_ok_and(|roundtrip| roundtrip == entry.path)
                            })
                    });
                    let Some(native) = native else {
                        errors.push(json!({
                        "path": display_path(path.inferred_native_path_string()),
                        "message": "unrepresentable native entry cannot be addressed losslessly",
                    }));
                        return None;
                    };
                    let mut value = json!({"path": display_path(native.into_string()), "kind": entry.kind});
                    if options.include_metadata {
                        if let Some(metadata) = &entry.metadata {
                            value["metadata"] = json!(metadata);
                        }
                        if let Some(error) = &entry.metadata_error {
                            value["metadata_error"] = json!(error);
                        }
                    }
                    Some(value)
                })
                .collect::<Vec<_>>();
            let complete = !outcome.truncated && errors.is_empty();
            let mut value = json!({
                "path": path.inferred_native_path_string(),
                "environment_id": environment.environment_id,
                "effective_walk_options": {
                    "max_depth": options.max_depth,
                    "max_entries": options.max_entries,
                    "max_directories": options.max_directories,
                    "follow_directory_symlinks": options.follow_directory_symlinks,
                    "include_metadata": options.include_metadata,
                    "summary_only": options.summary_only,
                    "prune_hidden_directories": options.prune_hidden_directories,
                    "filters": options.filters,
                    "max_response_bytes": codex_exec_server::MAX_WALK_RESPONSE_BYTES,
                    "apply_gitignore": options.apply_gitignore,
                },
                "limits_clamped": args.max_depth.is_some_and(|value| value > MAX_DEPTH)
                    || args.max_entries.is_some_and(|value| value > MAX_ENTRIES)
                    || args.max_directories.is_some_and(|value| value > 10_000),
                "entries": entries,
                "errors": errors,
                "truncated": outcome.truncated,
                "unexplored": outcome.unexplored.iter().map(|stop| json!({
                    "path": display_path(stop.path.inferred_native_path_string()), "reason": stop.reason,
                    "effectiveLimit": stop.effective_limit, "partiallyExamined": stop.partially_examined,
                })).collect::<Vec<_>>(),
                "continuation_policy": "Re-examine unexplored paths with adjusted limits; directories are mutable, not stable resumable pages.",
                "complete": complete,
            });
            if let Some(summary) = &outcome.summary {
                value["summary"] = json!({
                    "files": summary.files,
                    "directories": summary.directories,
                    "file_bytes": summary.file_bytes,
                    "entries_examined": summary.entries_examined,
                    "directories_read": summary.directories_read,
                    "skipped": summary.skipped,
                    "totals_complete": complete,
                    "scope": "Eligible descendants under effective_walk_options, excluding the root and skipped entries/subtrees; logical file bytes counted per path, not allocated storage or unique objects.",
                    "symlink_policy": "Directory links are omitted and never followed; file links count link sizes per path without probing targets.",
                });
            } else if outcome.gitignored_entries > 0 {
                // Summaries already report this as summary.skipped.gitignored.
                value["gitignored_omitted"] = json!(outcome.gitignored_entries);
            }
            let evidence = crate::tools::context::semantic_evidence_sampling_signal(json!({
                "source": "list_files",
                "scope": {"path": value["path"], "environment_id": value["environment_id"],
                    "walk_options": value["effective_walk_options"]},
                "identity": crate::tool_history::sha256(value.to_string().as_bytes()),
            }));
            Ok(boxed_tool_output(
                JsonToolOutput::new(value).with_sampling_request_signal(evidence),
            ))
        })
    }
}

impl CoreToolRuntime for ListFilesHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }

    fn permits_shared_workspace_observation(&self, _payload: &ToolPayload) -> bool {
        true
    }
}

