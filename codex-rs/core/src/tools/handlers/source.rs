use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::source_spec::LOCATE_TASK_TOOL_NAME;
use crate::tools::handlers::source_spec::READ_FILE_SPAN_TOOL_NAME;
use crate::tools::handlers::source_spec::SEARCH_SOURCE_TOOL_NAME;
use crate::tools::handlers::source_spec::SourceToolOptions;
use crate::tools::handlers::source_spec::create_locate_task_tool;
use crate::tools::handlers::source_spec::create_read_file_span_tool;
use crate::tools::handlers::source_spec::create_search_source_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_agent_task_store::WorkspaceActorKind;
use codex_agent_task_store::WorkspaceActorRegistration;
use codex_agent_task_store::WorkspaceManifestEntry;
use codex_agent_task_store::WorkspaceStrategy;
use codex_file_search::source_search::ReadFileSpanOutput;
use codex_file_search::source_search::SOURCE_READ_DEFAULT_LINES;
use codex_file_search::source_search::SOURCE_SEARCH_DEFAULT_MAX_MATCHES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_FILE_BYTES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_ROOTS;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_WALK_DEPTH;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_WALK_DIRECTORIES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_WALK_ENTRIES;
use codex_file_search::source_search::SourceIgnoreMatcher;
use codex_file_search::source_search::SourceSearchAccumulator;
use codex_file_search::source_search::SourceSearchOptions;
use codex_file_search::source_search::SourceSearchOutput;
use codex_file_search::source_search::read_file_span_from_bytes;
use codex_file_search::source_search::should_descend_source_path;
use codex_file_search::source_search::should_scan_source_file;
use codex_file_search::source_search::validate_read_file_span_bounds;
use codex_file_search::task_locator::LOCATE_TASK_MAX_FILES;
use codex_file_search::task_locator::LOCATE_TASK_MAX_SOURCE_BYTES;
use codex_file_search::task_locator::LocateTaskRequest;
use codex_file_search::task_locator::locate_task_cancellable;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::FileMetadata;
use codex_file_system::FileSystemSandboxContext;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::future::Future;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tracing::warn;

#[cfg(test)]
pub(crate) mod test_observation {
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[derive(Clone, Default)]
    struct Counters {
        successful_content_reads: Arc<AtomicUsize>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) struct Snapshot {
        pub successful_content_reads: usize,
    }

    tokio::task_local! {
        static COUNTERS: Counters;
    }

    pub(crate) async fn observe<F: Future>(future: F) -> (F::Output, Snapshot) {
        let counters = Counters::default();
        let output = COUNTERS.scope(counters.clone(), future).await;
        let snapshot = Snapshot {
            successful_content_reads: counters.successful_content_reads.load(Ordering::Relaxed),
        };
        (output, snapshot)
    }

    pub(super) fn record_successful_content_read() {
        let _ = COUNTERS.try_with(|counters| {
            counters
                .successful_content_reads
                .fetch_add(1, Ordering::Relaxed);
        });
    }
}

const SOURCE_TOOL_MAX_RENDERED_BYTES: usize = 8 * 1024;
const SOURCE_COORDINATION_MAX_WAIT: Duration = Duration::from_millis(100);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocateTaskArgs {
    task: String,
    #[serde(default)]
    path_anchor: Option<String>,
    #[serde(default)]
    symbol_anchor: Option<String>,
    #[serde(default)]
    max_files: Option<usize>,
    #[serde(default)]
    max_source_bytes: Option<usize>,
    #[serde(default)]
    force_fresh: bool,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchSourceArgs {
    query: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    context_lines: Option<usize>,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    include_generated: bool,
    #[serde(default)]
    include_vendor: bool,
    #[serde(default)]
    include_locks: bool,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileSpanArgs {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    line_count: Option<usize>,
    /// Execute normally rather than reusing prior immutable evidence.
    #[serde(default)]
    force_fresh: bool,
    #[serde(default)]
    environment_id: Option<String>,
}

pub struct LocateTaskHandler {
    options: SourceToolOptions,
}

impl LocateTaskHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        Self {
            options: SourceToolOptions {
                include_environment_id,
            },
        }
    }
}

pub struct SearchSourceHandler {
    options: SourceToolOptions,
}

impl SearchSourceHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        Self {
            options: SourceToolOptions {
                include_environment_id,
            },
        }
    }
}

pub struct ReadFileSpanHandler {
    options: SourceToolOptions,
}

impl ReadFileSpanHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        Self {
            options: SourceToolOptions {
                include_environment_id,
            },
        }
    }
}

impl ToolExecutor<ToolInvocation> for SearchSourceHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(SEARCH_SOURCE_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_search_source_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(handle_search_source(invocation, self.options))
    }
}

impl CoreToolRuntime for SearchSourceHandler {}

impl ToolExecutor<ToolInvocation> for LocateTaskHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(LOCATE_TASK_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_locate_task_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(handle_locate_task(invocation, self.options))
    }
}

impl CoreToolRuntime for LocateTaskHandler {}

impl ToolExecutor<ToolInvocation> for ReadFileSpanHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_FILE_SPAN_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_read_file_span_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(handle_read_file_span(invocation, self.options))
    }
}

impl CoreToolRuntime for ReadFileSpanHandler {}

async fn handle_locate_task(
    invocation: ToolInvocation,
    tool_options: SourceToolOptions,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "locate_task received unsupported payload".to_string(),
        ));
    };
    let args: LocateTaskArgs = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse locate_task arguments: {err}"))
    })?;
    reject_unadvertised_environment_id(
        LOCATE_TASK_TOOL_NAME,
        tool_options,
        args.environment_id.as_deref(),
    )?;
    if args.task.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "locate_task requires a nonempty task description".to_string(),
        ));
    }
    let max_files = args.max_files.unwrap_or(LOCATE_TASK_MAX_FILES);
    if max_files == 0 || max_files > LOCATE_TASK_MAX_FILES {
        return Err(FunctionCallError::RespondToModel(format!(
            "locate_task max_files must be between 1 and {LOCATE_TASK_MAX_FILES}"
        )));
    }
    let max_source_bytes = args
        .max_source_bytes
        .unwrap_or(LOCATE_TASK_MAX_SOURCE_BYTES);
    if max_source_bytes == 0 || max_source_bytes > LOCATE_TASK_MAX_SOURCE_BYTES {
        return Err(FunctionCallError::RespondToModel(format!(
            "locate_task max_source_bytes must be between 1 and {LOCATE_TASK_MAX_SOURCE_BYTES}"
        )));
    }

    let source_context = local_source_context(&invocation, args.environment_id.as_deref()).await?;
    let repository_root = source_context.repo_root_abs.as_path().to_path_buf();
    let cache_root = invocation
        .step_context
        .turn
        .config
        .codex_home
        .as_path()
        .to_path_buf();
    let manifest_path = repository_root.join("source_owners.toml");
    let environment_id = args.environment_id.clone();
    let task = args.task;
    let path_anchor = args.path_anchor;
    let symbol_anchor = args.symbol_anchor;
    let force_fresh = args.force_fresh;
    let cancelled = Arc::new(AtomicBool::new(false));
    let blocking_cancelled = Arc::clone(&cancelled);
    let mut indexing_task = tokio::task::spawn_blocking(move || {
        locate_task_cancellable(
            &LocateTaskRequest {
                repository_root: &repository_root,
                cache_root: &cache_root,
                manifest_path: &manifest_path,
                environment_id: environment_id.as_deref(),
                task: &task,
                path_anchor: path_anchor.as_deref(),
                symbol_anchor: symbol_anchor.as_deref(),
                max_files,
                max_source_bytes,
                force_fresh,
            },
            &blocking_cancelled,
        )
    });
    let indexing_result = tokio::select! {
        result = &mut indexing_task => result,
        _ = invocation.cancellation_token.cancelled() => {
            cancelled.store(true, Ordering::Release);
            let _ = indexing_task.await;
            return Err(FunctionCallError::RespondToModel(
                "locate_task was cancelled".to_string(),
            ));
        }
    };
    let output = indexing_result
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("locate_task indexing task failed: {err}"))
        })?
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("locate_task query failed: {err:#}"))
        })?;

    let supporting_entries = output
        .supporting_reads
        .into_iter()
        .map(|read| WorkspaceManifestEntry {
            path: read.path,
            content_hash: Some(read.content_hash),
            existed: true,
        })
        .collect();
    record_supporting_source_reads(&invocation, &source_context, supporting_entries).await?;

    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        output.rendered,
        Some(true),
    )))
}

async fn handle_search_source(
    invocation: ToolInvocation,
    tool_options: SourceToolOptions,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "search_source received unsupported payload".to_string(),
        ));
    };
    let args: SearchSourceArgs = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse search_source arguments: {err}"))
    })?;
    reject_unadvertised_environment_id(
        SEARCH_SOURCE_TOOL_NAME,
        tool_options,
        args.environment_id.as_deref(),
    )?;
    let mut options = SourceSearchOptions::new(PathBuf::new(), args.query);
    options.roots = args.paths.into_iter().map(PathBuf::from).collect();
    options.max_matches = args
        .max_results
        .unwrap_or(SOURCE_SEARCH_DEFAULT_MAX_MATCHES);
    options.context_lines = args.context_lines.unwrap_or(0);
    options.case_sensitive = args.case_sensitive;
    options.include_generated = args.include_generated;
    options.include_vendor = args.include_vendor;
    options.include_locks = args.include_locks;
    validate_search_root_count(&options.roots)?;
    let mut accumulator = SourceSearchAccumulator::new(&options)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
    let source_context = local_source_context(&invocation, args.environment_id.as_deref()).await?;
    let recover_explicit_root_failures = !options.roots.is_empty();
    let roots = resolve_search_roots(&source_context, &options.roots).await?;
    let ignore_matcher = SourceIgnoreMatcher::new_preloaded(
        source_context
            .is_git_repository
            .then_some(source_context.repo_root_abs.as_path()),
    );
    load_repository_exclude_rules(&source_context, &ignore_matcher).await?;
    // The filesystem executor does not currently expose the selected
    // environment's Git configuration or home directory. Reading the host's
    // values here mixes environments and can incorrectly hide results, so omit
    // global excludes explicitly until that contract is available.
    let omitted_global_ignore = source_context.is_git_repository;
    let mut observed_entries = BTreeMap::new();
    let traversal_started = Instant::now();
    let scan_result = scan_source_roots(
        &source_context,
        &roots,
        &options,
        &ignore_matcher,
        &mut accumulator,
        &mut observed_entries,
        recover_explicit_root_failures,
    )
    .await;
    accumulator.record_traversal_duration(traversal_started.elapsed());
    scan_result?;
    let output = accumulator.finish(
        roots
            .iter()
            .map(|root| relative_source_path(&source_context, root))
            .collect::<Result<Vec<_>, _>>()?,
    );
    let mut supporting_paths = output
        .matches
        .iter()
        .map(|matched| matched.path.clone())
        .collect::<Vec<_>>();
    supporting_paths.sort();
    supporting_paths.dedup();
    let supporting_entries = supporting_paths
        .into_iter()
        .map(|path| {
            observed_entries.remove(&path).ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "search_source: no read-time manifest was retained for matched path {path}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    record_supporting_source_reads(&invocation, &source_context, supporting_entries).await?;

    let mut rendered = render_search_output(&output);
    if omitted_global_ignore {
        rendered.push_str(
            "\ndiagnostic: global Git ignore rules were omitted because the selected environment does not expose Git config resolution\n",
        );
    }
    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        rendered,
        Some(true),
    )))
}

async fn handle_read_file_span(
    invocation: ToolInvocation,
    tool_options: SourceToolOptions,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "read_file_span received unsupported payload".to_string(),
        ));
    };
    let args: ReadFileSpanArgs = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to parse read_file_span arguments: {err}"
        ))
    })?;
    reject_unadvertised_environment_id(
        READ_FILE_SPAN_TOOL_NAME,
        tool_options,
        args.environment_id.as_deref(),
    )?;
    let start_line = args.start_line.unwrap_or(1);
    let line_count = args.line_count.unwrap_or(SOURCE_READ_DEFAULT_LINES);
    validate_read_file_span_bounds(start_line, line_count)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
    if let Some((skill_path, bytes)) = read_loaded_skill_bytes(&invocation, &args.path).await? {
        let file_len = bytes.len();
        if file_len > SOURCE_SEARCH_MAX_FILE_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "source file `{}` is too large ({} bytes, max {})",
                args.path, file_len, SOURCE_SEARCH_MAX_FILE_BYTES
            )));
        }
        let output = read_file_span_from_bytes(skill_path, bytes, start_line, line_count)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        return Ok(boxed_tool_output(FunctionToolOutput::from_text(
            render_read_output(&output),
            Some(true),
        )));
    }
    let source_context = local_source_context(&invocation, args.environment_id.as_deref()).await?;
    let path = resolve_confined_path(&source_context, &args.path, "source file").await?;
    let relative_path = relative_source_path(&source_context, &path)?;
    // Bounded file reads are statically cheap and always execute the fresh-read path. Keep
    // accepting `force_fresh` for request compatibility even though both values now behave alike.
    let _ = args.force_fresh;
    let execution = async {
        let metadata = source_context
            .fs
            .get_metadata(&path, Some(&source_context.sandbox))
            .await
            .map_err(|err| source_fs_error("inspect", &path, err))?;
        if !metadata.is_file {
            return Err(FunctionCallError::RespondToModel(format!(
                "source path `{}` is not a file",
                args.path
            )));
        }
        let file_len = usize::try_from(metadata.size).unwrap_or(usize::MAX);
        if file_len > SOURCE_SEARCH_MAX_FILE_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "source file `{}` is too large ({} bytes, max {})",
                args.path, file_len, SOURCE_SEARCH_MAX_FILE_BYTES
            )));
        }
        let Some(bytes) = read_source_file_stably(&source_context, &path, &metadata).await? else {
            return Err(FunctionCallError::RespondToModel(format!(
                "source file `{}` changed while it was being read; retry the read",
                args.path
            )));
        };
        let supporting_entry = manifest_entry_from_bytes(relative_path.clone(), &bytes);
        let output = read_file_span_from_bytes(relative_path, bytes, start_line, line_count)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        record_supporting_source_reads(&invocation, &source_context, vec![supporting_entry])
            .await?;
        Ok(render_read_output(&output))
    }
    .await;
    let rendered = execution?;

    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        rendered,
        Some(true),
    )))
}

async fn read_loaded_skill_bytes(
    invocation: &ToolInvocation,
    requested_path: &str,
) -> Result<Option<(String, Vec<u8>)>, FunctionCallError> {
    let Ok(requested_path) = AbsolutePathBuf::try_from(requested_path) else {
        return Ok(None);
    };
    let snapshot = &invocation.step_context.turn.turn_skills.snapshot;
    let outcome = snapshot.outcome();
    let Some(skill) = outcome
        .skills
        .iter()
        .find(|skill| outcome.is_skill_enabled(skill) && skill.path_to_skills_md == requested_path)
    else {
        return Ok(None);
    };
    let contents = snapshot.read_skill_text(skill).await.map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "unable to read loaded skill `{}`: {err}",
            skill.path_to_skills_md.display()
        ))
    })?;
    let display_path = skill.path_to_skills_md.to_string_lossy().replace('\\', "/");
    Ok(Some((display_path, contents.into_bytes())))
}

async fn record_supporting_source_reads(
    invocation: &ToolInvocation,
    source_context: &LocalSourceContext,
    entries: Vec<WorkspaceManifestEntry>,
) -> Result<(), FunctionCallError> {
    await_supporting_read_coordination(
        SOURCE_COORDINATION_MAX_WAIT,
        record_supporting_source_reads_inner(invocation, source_context, entries),
    )
    .await
}

async fn await_supporting_read_coordination<F>(
    max_wait: Duration,
    coordination: F,
) -> Result<(), FunctionCallError>
where
    F: Future<Output = Result<(), FunctionCallError>>,
{
    match tokio::time::timeout(max_wait, coordination).await {
        Ok(result) => result,
        Err(_) => {
            warn!(
                max_wait_ms = max_wait.as_millis(),
                "source read coordination exceeded its time budget; returning confined read output"
            );
            Ok(())
        }
    }
}

async fn record_supporting_source_reads_inner(
    invocation: &ToolInvocation,
    source_context: &LocalSourceContext,
    entries: Vec<WorkspaceManifestEntry>,
) -> Result<(), FunctionCallError> {
    if entries.is_empty() {
        return Ok(());
    }
    let coordinator = invocation.session.services.agent_control.task_coordinator();
    if coordinator.store().is_none()
        && let Err(error) = coordinator
            .initialize_for_workspace_coordination(
                invocation.session.services.state_db.clone(),
                invocation.step_context.turn.config.sqlite_home.clone(),
                invocation
                    .step_context
                    .turn
                    .config
                    .model_provider_id
                    .clone(),
                invocation
                    .session
                    .services
                    .agent_control
                    .session_id()
                    .to_string(),
            )
            .await
    {
        warn!(
            %error,
            "source tool completed without initializing supporting-read coordination"
        );
        return Ok(());
    }
    let binding = coordinator.binding_for_source(&invocation.step_context.turn.session_source);
    let Some(store) = coordinator.store() else {
        warn!("source tool completed without an available supporting-read task store");
        return Ok(());
    };
    let Some(root_session_id) = coordinator.root_session_id() else {
        warn!("source tool completed without a durable root task identity");
        return Ok(());
    };
    let agent_path = invocation
        .step_context
        .turn
        .session_source
        .get_agent_path()
        .map(|path| path.to_string())
        .unwrap_or_else(|| "/root".to_string());
    let (actor_id, kind) = if let Some(binding) = binding.as_ref() {
        (
            format!("attempt:{}", binding.attempt_id),
            WorkspaceActorKind::Typed,
        )
    } else if invocation
        .step_context
        .turn
        .session_source
        .is_non_root_agent()
    {
        (
            format!("legacy:{root_session_id}:{agent_path}"),
            WorkspaceActorKind::Legacy,
        )
    } else {
        (format!("root:{root_session_id}"), WorkspaceActorKind::Root)
    };
    if let Some(binding) = binding.as_ref() {
        match coordinator.heartbeat_typed_actor_binding(binding).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(FunctionCallError::RespondToModel(
                    "read_file_span: the bound typed assignment attempt is no longer active"
                        .to_string(),
                ));
            }
            Err(error) => {
                warn!(
                    %error,
                    "source tool completed without persisting the typed reader heartbeat"
                );
                return Ok(());
            }
        }
    }
    if kind != WorkspaceActorKind::Typed
        && let Err(error) = store
            .register_workspace_actor(
                source_context.repo_root_abs.as_path(),
                WorkspaceActorRegistration {
                    root_session_id,
                    actor_id: actor_id.clone(),
                    kind,
                    assignment_id: None,
                    attempt_id: None,
                    strategy: WorkspaceStrategy::Shared,
                },
            )
            .await
    {
        warn!(
            %error,
            "source tool completed without registering its durable reader identity"
        );
        return Ok(());
    }
    if let Err(error) = store
        .record_supporting_read_entries(source_context.repo_root_abs.as_path(), actor_id, entries)
        .await
    {
        warn!(
            %error,
            "source tool completed without persisting its supporting-read manifest"
        );
    }
    Ok(())
}

fn reject_unadvertised_environment_id(
    tool_name: &str,
    options: SourceToolOptions,
    environment_id: Option<&str>,
) -> Result<(), FunctionCallError> {
    if !options.include_environment_id && environment_id.is_some() {
        return Err(FunctionCallError::RespondToModel(format!(
            "failed to parse {tool_name} arguments: unknown field `environment_id`"
        )));
    }
    Ok(())
}

struct LocalSourceContext {
    fs: Arc<dyn ExecutorFileSystem>,
    sandbox: FileSystemSandboxContext,
    repo_root: PathUri,
    repo_root_abs: AbsolutePathBuf,
    is_git_repository: bool,
}

async fn local_source_context(
    invocation: &ToolInvocation,
    environment_id: Option<&str>,
) -> Result<LocalSourceContext, FunctionCallError> {
    let environment =
        resolve_tool_environment(&invocation.step_context.environments, environment_id)?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "source tools require a selected local environment".to_string(),
                )
            })?;
    if environment.environment.is_remote() {
        return Err(FunctionCallError::RespondToModel(
            "source tools currently support local environments only".to_string(),
        ));
    }
    let sandbox = invocation
        .step_context
        .turn
        .file_system_sandbox_context(/*additional_permissions*/ None, environment.cwd());
    let fs = environment.environment.get_filesystem();
    let cwd = fs
        .canonicalize(environment.cwd(), Some(&sandbox))
        .await
        .map_err(|err| source_fs_error("canonicalize", environment.cwd(), err))?;
    let cwd_metadata = fs
        .get_metadata(&cwd, Some(&sandbox))
        .await
        .map_err(|err| source_fs_error("inspect", &cwd, err))?;
    if !cwd_metadata.is_directory {
        return Err(FunctionCallError::RespondToModel(format!(
            "source tool cwd `{}` is not a directory",
            cwd.inferred_native_path_string()
        )));
    }
    let (repo_root, is_git_repository) = find_repo_root(fs.as_ref(), &sandbox, &cwd).await?;
    let repo_root_abs = repo_root.to_abs_path().map_err(|err| {
        FunctionCallError::RespondToModel(format!("source repo root is not host-native: {err}"))
    })?;
    Ok(LocalSourceContext {
        fs,
        sandbox,
        repo_root,
        repo_root_abs,
        is_git_repository,
    })
}

async fn find_repo_root(
    fs: &dyn ExecutorFileSystem,
    sandbox: &FileSystemSandboxContext,
    cwd: &PathUri,
) -> Result<(PathUri, bool), FunctionCallError> {
    for ancestor in cwd.ancestors() {
        let dot_git = ancestor.join(".git").map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "unable to resolve repository marker below `{}`: {err}",
                ancestor.inferred_native_path_string()
            ))
        })?;
        match fs.get_metadata(&dot_git, Some(sandbox)).await {
            Ok(metadata) if metadata.is_directory || metadata.is_file => {
                return Ok((ancestor, true));
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(source_fs_error("inspect", &dot_git, err)),
        }
    }
    Ok((cwd.clone(), false))
}

async fn resolve_search_roots(
    context: &LocalSourceContext,
    roots: &[PathBuf],
) -> Result<Vec<PathUri>, FunctionCallError> {
    validate_search_root_count(roots)?;
    let mut roots = if roots.is_empty() {
        vec![context.repo_root.clone()]
    } else {
        let mut resolved = Vec::with_capacity(roots.len());
        for root in roots {
            resolved.push(
                resolve_confined_path(context, &root.to_string_lossy(), "source root").await?,
            );
        }
        resolved
    };
    roots.sort_by(|left, right| {
        left.ancestors()
            .count()
            .cmp(&right.ancestors().count())
            .then_with(|| left.to_string().cmp(&right.to_string()))
    });
    roots.dedup();
    let mut deduped = Vec::<PathUri>::new();
    for root in roots {
        if deduped.iter().any(|parent| root.starts_with(parent)) {
            continue;
        }
        deduped.push(root);
    }
    Ok(deduped)
}

fn validate_search_root_count(roots: &[PathBuf]) -> Result<(), FunctionCallError> {
    if roots.len() > SOURCE_SEARCH_MAX_ROOTS {
        return Err(FunctionCallError::RespondToModel(format!(
            "too many source roots ({} provided, max {})",
            roots.len(),
            SOURCE_SEARCH_MAX_ROOTS
        )));
    }
    Ok(())
}

async fn resolve_confined_path(
    context: &LocalSourceContext,
    path: &str,
    label: &str,
) -> Result<PathUri, FunctionCallError> {
    let candidate = context.repo_root.join(path).map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve {label} `{path}`: {err}"))
    })?;
    let canonical = context
        .fs
        .canonicalize(&candidate, Some(&context.sandbox))
        .await
        .map_err(|err| source_fs_error("canonicalize", &candidate, err))?;
    if !canonical.starts_with(&context.repo_root) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{label} `{path}` resolves outside repository root `{}`",
            context.repo_root.inferred_native_path_string()
        )));
    }
    Ok(canonical)
}

async fn load_repository_exclude_rules(
    context: &LocalSourceContext,
    ignore_matcher: &SourceIgnoreMatcher,
) -> Result<(), FunctionCallError> {
    if !context.is_git_repository {
        return Ok(());
    }
    let dot_git = context.repo_root.join(".git").map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "unable to resolve repository ignore metadata: {err}"
        ))
    })?;
    let git_common_directory = match context
        .fs
        .get_metadata(&dot_git, Some(&context.sandbox))
        .await
    {
        Ok(metadata) if metadata.is_directory => Some(dot_git),
        Ok(metadata) if metadata.is_file => resolve_git_common_directory(context, &dot_git).await?,
        Ok(_) => None,
        Err(err) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "unable to inspect repository ignore metadata `{dot_git}`: {err}"
            )));
        }
    };
    let Some(git_common_directory) = git_common_directory else {
        return Ok(());
    };
    let Some(exclude_path) = git_common_directory.join("info/exclude").ok() else {
        return Ok(());
    };
    let Some(contents) = read_optional_ignore_text(context, &exclude_path).await? else {
        return Ok(());
    };
    let source_path = exclude_path.to_abs_path().map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "repository exclude path is not host-native: {err}"
        ))
    })?;
    ignore_matcher.set_repository_exclude(
        context.repo_root_abs.as_path(),
        source_path.as_path(),
        &contents,
    );
    Ok(())
}

async fn resolve_git_common_directory(
    context: &LocalSourceContext,
    dot_git: &PathUri,
) -> Result<Option<PathUri>, FunctionCallError> {
    let Some(contents) = read_optional_ignore_text(context, dot_git).await? else {
        return Ok(None);
    };
    let Some(git_dir_target) = contents.strip_prefix("gitdir:").map(str::trim) else {
        return Ok(None);
    };
    if git_dir_target.is_empty() {
        return Ok(None);
    }
    let git_directory = context.repo_root.join(git_dir_target).map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve Git directory: {err}"))
    })?;
    let git_directory = context
        .fs
        .canonicalize(&git_directory, Some(&context.sandbox))
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "unable to canonicalize Git directory: {err}"
            ))
        })?;
    let common_dir_path = git_directory.join("commondir").map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve Git common directory: {err}"))
    })?;
    let Some(common_dir) = read_optional_ignore_text(context, &common_dir_path).await? else {
        return Ok(Some(git_directory));
    };
    let common_dir = common_dir.trim();
    if common_dir.is_empty() {
        return Ok(Some(git_directory));
    }
    let common_directory = git_directory.join(common_dir).map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve Git common directory: {err}"))
    })?;
    let common_directory = context
        .fs
        .canonicalize(&common_directory, Some(&context.sandbox))
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "unable to canonicalize Git common directory: {err}"
            ))
        })?;
    Ok(Some(common_directory))
}

async fn read_optional_ignore_text(
    context: &LocalSourceContext,
    path: &PathUri,
) -> Result<Option<String>, FunctionCallError> {
    let read_result = if path.starts_with(&context.repo_root) {
        context
            .fs
            .read_file_bounded_confined(
                path,
                &context.repo_root,
                SOURCE_SEARCH_MAX_FILE_BYTES,
                Some(&context.sandbox),
            )
            .await
    } else {
        context
            .fs
            .read_file_bounded(path, SOURCE_SEARCH_MAX_FILE_BYTES, Some(&context.sandbox))
            .await
    };
    let bytes = match read_result {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "unable to read optional source ignore file `{path}`: {err}"
            )));
        }
    };
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    String::from_utf8(bytes).map(Some).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "optional source ignore file `{path}` is not UTF-8: {err}"
        ))
    })
}

async fn load_directory_ignore_rules(
    context: &LocalSourceContext,
    directory: &PathUri,
    ignore_matcher: &SourceIgnoreMatcher,
) -> Result<(), FunctionCallError> {
    let directory_path = directory.to_abs_path().map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "source ignore directory is not host-native: {err}"
        ))
    })?;
    if ignore_matcher.has_directory_rules(directory_path.as_path()) {
        return Ok(());
    }
    let ignore_path = directory.join(".ignore").map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve .ignore path: {err}"))
    })?;
    let git_ignore_path = directory.join(".gitignore").map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve .gitignore path: {err}"))
    })?;
    let ignore_contents = read_optional_ignore_text(context, &ignore_path).await?;
    let git_ignore_contents = read_optional_ignore_text(context, &git_ignore_path).await?;
    ignore_matcher.add_directory_rules(
        directory_path.as_path(),
        ignore_contents.as_deref(),
        git_ignore_contents.as_deref(),
    );
    Ok(())
}

async fn load_ignore_rules_through(
    context: &LocalSourceContext,
    directory: &PathUri,
    ignore_matcher: &SourceIgnoreMatcher,
) -> Result<(), FunctionCallError> {
    let mut ancestors = directory
        .ancestors()
        .take_while(|ancestor| ancestor.starts_with(&context.repo_root))
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        load_directory_ignore_rules(context, &ancestor, ignore_matcher).await?;
    }
    Ok(())
}

fn source_path_is_ignored(
    path: &PathUri,
    is_directory: bool,
    ignore_matcher: &SourceIgnoreMatcher,
) -> Result<bool, FunctionCallError> {
    let path = path.to_abs_path().map_err(|err| {
        FunctionCallError::RespondToModel(format!("source ignore path is not host-native: {err}"))
    })?;
    Ok(ignore_matcher.is_ignored(path.as_path(), is_directory))
}

async fn scan_source_root(
    context: &LocalSourceContext,
    root: &PathUri,
    options: &SourceSearchOptions,
    ignore_matcher: &SourceIgnoreMatcher,
    accumulator: &mut SourceSearchAccumulator,
    observed_entries: &mut BTreeMap<String, WorkspaceManifestEntry>,
) -> Result<(), FunctionCallError> {
    let metadata = context
        .fs
        .get_metadata(root, Some(&context.sandbox))
        .await
        .map_err(|err| source_fs_error("inspect", root, err))?;
    if metadata.is_file {
        return add_source_file(context, root, accumulator, observed_entries, false).await;
    }
    if !metadata.is_directory {
        return Err(FunctionCallError::RespondToModel(format!(
            "source root `{}` is neither a file nor a directory",
            root.inferred_native_path_string()
        )));
    }
    load_ignore_rules_through(context, root, ignore_matcher).await?;
    if root != &context.repo_root && source_path_is_ignored(root, true, ignore_matcher)? {
        return Ok(());
    }

    let mut queue = VecDeque::from([(root.clone(), 0usize)]);
    while let Some((directory, depth)) = queue.pop_front() {
        if accumulator.should_stop() {
            break;
        }
        if !accumulator.reserve_walk_directory(SOURCE_SEARCH_MAX_WALK_DIRECTORIES) {
            break;
        }
        load_directory_ignore_rules(context, &directory, ignore_matcher).await?;
        let remaining_entries = accumulator.remaining_walk_entries(SOURCE_SEARCH_MAX_WALK_ENTRIES);
        if remaining_entries == 0 {
            accumulator.mark_walk_limit();
            return Ok(());
        }
        let entries_result = context
            .fs
            .read_directory_bounded(&directory, remaining_entries, Some(&context.sandbox))
            .await;
        let outcome = if depth == 0 {
            entries_result.map_err(|err| source_fs_error("read directory", &directory, err))?
        } else {
            let Some(outcome) = recover_scan_result(entries_result, accumulator) else {
                continue;
            };
            outcome
        };
        if outcome.entries_examined > remaining_entries
            || outcome.entries.len() > outcome.entries_examined
        {
            return Err(FunctionCallError::RespondToModel(
                "bounded directory read returned an invalid entry count".to_string(),
            ));
        }
        accumulator.record_walk_entries(outcome.entries_examined, SOURCE_SEARCH_MAX_WALK_ENTRIES);
        let limit_reached = outcome.limit_reached;
        let mut entries = outcome.entries;
        entries.sort_by(|left, right| left.file_name.cmp(&right.file_name));

        for entry in entries {
            if accumulator.should_stop() {
                break;
            }
            let Some(child) = recover_scan_result(directory.join(&entry.file_name), accumulator)
            else {
                continue;
            };
            if entry.is_directory {
                let Some(child_metadata) = recover_scan_result(
                    context
                        .fs
                        .get_metadata(&child, Some(&context.sandbox))
                        .await,
                    accumulator,
                ) else {
                    continue;
                };
                let Some(relative) =
                    recover_scan_result(relative_source_path(context, &child), accumulator)
                else {
                    continue;
                };
                if !child_metadata.is_directory
                    || child_metadata.is_symlink
                    || !should_descend_source_path(
                        Path::new(&relative),
                        options.include_generated,
                        options.include_vendor,
                    )
                    || source_path_is_ignored(&child, true, ignore_matcher)?
                {
                    accumulator.record_ignored_entries(1);
                    continue;
                }
                if depth >= SOURCE_SEARCH_MAX_WALK_DEPTH {
                    accumulator.mark_walk_limit();
                    continue;
                }
                queue.push_back((child, depth.saturating_add(1)));
                continue;
            }
            if !entry.is_file {
                accumulator.record_ignored_entries(1);
                continue;
            }
            let Some(relative) =
                recover_scan_result(relative_source_path(context, &child), accumulator)
            else {
                continue;
            };
            if !should_scan_source_file(
                Path::new(&relative),
                options.include_generated,
                options.include_vendor,
                options.include_locks,
            ) || source_path_is_ignored(&child, false, ignore_matcher)?
            {
                accumulator.record_ignored_entries(1);
                continue;
            }
            let Some(canonical) = recover_scan_result(
                context
                    .fs
                    .canonicalize(&child, Some(&context.sandbox))
                    .await,
                accumulator,
            ) else {
                continue;
            };
            if !canonical.starts_with(&context.repo_root) {
                accumulator.mark_filesystem_error();
                continue;
            }
            let _ = recover_scan_result(
                add_source_file(context, &canonical, accumulator, observed_entries, true).await,
                accumulator,
            );
        }
        if limit_reached {
            accumulator.mark_walk_limit();
            return Ok(());
        }
    }
    Ok(())
}

async fn scan_source_roots(
    context: &LocalSourceContext,
    roots: &[PathUri],
    options: &SourceSearchOptions,
    ignore_matcher: &SourceIgnoreMatcher,
    accumulator: &mut SourceSearchAccumulator,
    observed_entries: &mut BTreeMap<String, WorkspaceManifestEntry>,
    recover_root_failures: bool,
) -> Result<(), FunctionCallError> {
    for root in roots {
        if accumulator.should_stop() {
            break;
        }
        let result = scan_source_root(
            context,
            root,
            options,
            ignore_matcher,
            accumulator,
            observed_entries,
        )
        .await;
        if recover_root_failures {
            let _ = recover_scan_result(result, accumulator);
        } else {
            result?;
        }
    }
    Ok(())
}

async fn add_source_file(
    context: &LocalSourceContext,
    path: &PathUri,
    accumulator: &mut SourceSearchAccumulator,
    observed_entries: &mut BTreeMap<String, WorkspaceManifestEntry>,
    already_filtered: bool,
) -> Result<(), FunctionCallError> {
    let started = Instant::now();
    let result = add_source_file_inner(
        context,
        path,
        accumulator,
        observed_entries,
        already_filtered,
    )
    .await;
    accumulator.record_file_scan_match_duration(started.elapsed());
    result
}

async fn add_source_file_inner(
    context: &LocalSourceContext,
    path: &PathUri,
    accumulator: &mut SourceSearchAccumulator,
    observed_entries: &mut BTreeMap<String, WorkspaceManifestEntry>,
    already_filtered: bool,
) -> Result<(), FunctionCallError> {
    let metadata = context
        .fs
        .get_metadata(path, Some(&context.sandbox))
        .await
        .map_err(|err| source_fs_error("inspect", path, err))?;
    if !metadata.is_file {
        return Err(FunctionCallError::RespondToModel(format!(
            "source path `{}` is not a file",
            path.inferred_native_path_string()
        )));
    }
    let relative = relative_source_path(context, path)?;
    let file_len = usize::try_from(metadata.size).unwrap_or(usize::MAX);
    let should_scan = if already_filtered {
        accumulator.consider_walked_file(file_len)
    } else {
        accumulator.consider_file(Path::new(&relative), file_len)
    };
    if !should_scan {
        return Ok(());
    }
    match read_source_file_stably(context, path, &metadata).await? {
        Some(bytes) => {
            observed_entries.insert(
                relative.clone(),
                manifest_entry_from_bytes(relative.clone(), &bytes),
            );
            accumulator.add_file_bytes(Path::new(&relative), bytes);
        }
        None => accumulator.mark_file_changed_during_read(),
    }
    Ok(())
}

fn manifest_entry_from_bytes(path: String, bytes: &[u8]) -> WorkspaceManifestEntry {
    WorkspaceManifestEntry {
        path,
        content_hash: Some(format!("{:x}", Sha256::digest(bytes))),
        existed: true,
    }
}

async fn read_source_file_stably(
    context: &LocalSourceContext,
    path: &PathUri,
    metadata_before: &FileMetadata,
) -> Result<Option<Vec<u8>>, FunctionCallError> {
    let expected_len = usize::try_from(metadata_before.size).unwrap_or(usize::MAX);
    let bytes = match context
        .fs
        .read_file_bounded_confined(
            path,
            &context.repo_root,
            SOURCE_SEARCH_MAX_FILE_BYTES,
            Some(&context.sandbox),
        )
        .await
    {
        Ok(bytes) => bytes,
        Err(err) if is_changed_file_race_error(err.kind()) => return Ok(None),
        Err(err) => return Err(source_fs_error("read", path, err)),
    };
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    #[cfg(test)]
    test_observation::record_successful_content_read();
    let metadata_after = match context.fs.get_metadata(path, Some(&context.sandbox)).await {
        Ok(metadata) => metadata,
        Err(err) if is_changed_file_race_error(err.kind()) => return Ok(None),
        Err(err) => return Err(source_fs_error("re-inspect", path, err)),
    };
    if bytes.len() != expected_len || source_metadata_changed(metadata_before, &metadata_after) {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn source_metadata_changed(before: &FileMetadata, after: &FileMetadata) -> bool {
    before.size != after.size
        || before.created_at_ms != after.created_at_ms
        || before.modified_at_ms != after.modified_at_ms
        || before.is_file != after.is_file
        || before.is_directory != after.is_directory
        || before.is_symlink != after.is_symlink
}

fn is_changed_file_race_error(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::NotFound | ErrorKind::InvalidInput)
}

fn recover_scan_result<T, E>(
    result: Result<T, E>,
    accumulator: &mut SourceSearchAccumulator,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(_) => {
            accumulator.mark_filesystem_error();
            None
        }
    }
}

fn relative_source_path(
    context: &LocalSourceContext,
    path: &PathUri,
) -> Result<String, FunctionCallError> {
    let path = path.to_abs_path().map_err(|err| {
        FunctionCallError::RespondToModel(format!("source path is not host-native: {err}"))
    })?;
    let relative = path.strip_prefix(&context.repo_root_abs).map_err(|_| {
        FunctionCallError::RespondToModel(format!(
            "source path `{}` is outside repository root `{}`",
            path.display(),
            context.repo_root_abs.display()
        ))
    })?;
    if relative.as_os_str().is_empty() {
        return Ok(".".to_string());
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn source_fs_error(action: &str, path: &PathUri, err: std::io::Error) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "unable to {action} source path `{}`: {err}",
        path.inferred_native_path_string()
    ))
}

fn render_search_output(output: &SourceSearchOutput) -> String {
    let mut rendered = BoundedSourceOutput::new();
    let _ = rendered.push_line("Source search evidence:".to_string());
    let _ = rendered.push_line(format!("query: {}", output.query));
    let coverage_line_index = rendered.line_count();
    let _ = rendered.push_line(render_search_coverage(output, output.truncated));
    if let Some(reason) = output.truncated_reason {
        let _ = rendered.push_line(format!("truncated_reason: {reason:?}"));
    }
    if let Some(note) = &output.coverage_note {
        let _ = rendered.push_line(format!("coverage_note: {note}"));
    }
    let first_match_micros = output
        .diagnostics
        .first_match_micros
        .map_or_else(|| "none".to_string(), |duration| duration.to_string());
    let _ = rendered.push_line(format!(
        "diagnostics: total_us={} first_match_us={first_match_micros} traversal_us={} file_scan_match_us={} projection_us={}",
        output.diagnostics.total_micros,
        output.diagnostics.traversal_micros,
        output.diagnostics.file_scan_match_micros,
        output.diagnostics.projection_micros,
    ));
    let render_reason_index = rendered.line_count();

    'matches: for source_match in &output.matches {
        let mut metadata = vec![
            String::new(),
            format!(
                "citation: {}:{}-{} (match line {})",
                source_match.path,
                source_match.start_line,
                source_match.end_line,
                source_match.line_number
            ),
        ];
        if let Some(route) = &source_match.source_map_route {
            metadata.push(format!("source_route: {route}"));
        }
        if !rendered.push_lines(metadata) {
            break;
        }
        for line in &source_match.lines {
            if !rendered.push_source_line(line.line_number, &line.text, line.text_truncated) {
                break 'matches;
            }
        }
    }

    rendered.finish(
        coverage_line_index,
        render_search_coverage(output, true),
        render_reason_index,
    )
}

fn render_search_coverage(output: &SourceSearchOutput, truncated: bool) -> String {
    format!(
        "coverage: complete={} walked={} ignored={} files={} skipped_too_large={} skipped_non_utf8={} changed_during_read={} filesystem_errors={} bytes={} total_matches={} returned={} truncated={truncated}",
        output.coverage_complete,
        output.coverage.walked_entries,
        output.coverage.ignored_entries,
        output.coverage.files_scanned,
        output.coverage.files_skipped_too_large,
        output.coverage.files_skipped_non_utf8,
        output.coverage.files_changed_during_read,
        output.coverage.filesystem_errors,
        output.coverage.bytes_scanned,
        output.coverage.total_matches,
        output.coverage.matches_returned,
    )
}

fn render_read_output(output: &ReadFileSpanOutput) -> String {
    let citation = match (output.start_line, output.end_line) {
        (Some(start), Some(end)) => format!("{}:{start}-{end}", output.path),
        _ => format!("{}:<empty>", output.path),
    };
    let mut rendered = BoundedSourceOutput::new();
    let _ = rendered.push_line("Source file evidence:".to_string());
    let _ = rendered.push_line(format!("citation: {citation}"));
    let summary_line_index = rendered.line_count();
    let _ = rendered.push_line(render_read_summary(output, output.truncated));
    let render_reason_index = rendered.line_count();
    if let Some(route) = &output.source_map_route
        && !rendered.push_line(format!("source_route: {route}"))
    {
        return rendered.finish(
            summary_line_index,
            render_read_summary(output, true),
            render_reason_index,
        );
    }
    for line in &output.lines {
        if !rendered.push_source_line(line.line_number, &line.text, line.text_truncated) {
            break;
        }
    }

    rendered.finish(
        summary_line_index,
        render_read_summary(output, true),
        render_reason_index,
    )
}

fn render_read_summary(output: &ReadFileSpanOutput, truncated: bool) -> String {
    format!(
        "total_lines: {} bytes_returned: {} truncated: {truncated}",
        output.total_lines, output.bytes_returned,
    )
}

struct BoundedSourceOutput {
    lines: Vec<String>,
    rendered_bytes: usize,
    content_limit: usize,
    render_truncated: bool,
}

impl BoundedSourceOutput {
    fn new() -> Self {
        let marker = source_output_truncation_marker();
        let reserved_bytes = "\nrender_truncated_reason: MaxRenderedBytes"
            .len()
            .saturating_add(1)
            .saturating_add(marker.len());
        Self {
            lines: Vec::new(),
            rendered_bytes: 0,
            content_limit: SOURCE_TOOL_MAX_RENDERED_BYTES.saturating_sub(reserved_bytes),
            render_truncated: false,
        }
    }

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn push_line(&mut self, line: String) -> bool {
        let separator_bytes = usize::from(!self.lines.is_empty());
        let additional_bytes = separator_bytes.saturating_add(line.len());
        if self.rendered_bytes.saturating_add(additional_bytes) > self.content_limit {
            self.render_truncated = true;
            return false;
        }
        self.rendered_bytes = self.rendered_bytes.saturating_add(additional_bytes);
        self.lines.push(line);
        true
    }

    fn push_lines(&mut self, lines: Vec<String>) -> bool {
        let additional_bytes = lines
            .iter()
            .enumerate()
            .fold(0usize, |total, (index, line)| {
                total
                    .saturating_add(usize::from(!self.lines.is_empty() || index > 0))
                    .saturating_add(line.len())
            });
        if self.rendered_bytes.saturating_add(additional_bytes) > self.content_limit {
            self.render_truncated = true;
            return false;
        }
        self.rendered_bytes = self.rendered_bytes.saturating_add(additional_bytes);
        self.lines.extend(lines);
        true
    }

    fn push_source_line(&mut self, line_number: usize, text: &str, text_truncated: bool) -> bool {
        let prefix = format!("{line_number:>6} | ");
        let suffix = if text_truncated {
            " [line truncated]"
        } else {
            ""
        };
        let separator_bytes = usize::from(!self.lines.is_empty());
        let full_bytes = separator_bytes
            .saturating_add(prefix.len())
            .saturating_add(text.len())
            .saturating_add(suffix.len());
        if self.rendered_bytes.saturating_add(full_bytes) <= self.content_limit {
            let mut line = String::with_capacity(prefix.len() + text.len() + suffix.len());
            line.push_str(&prefix);
            line.push_str(text);
            line.push_str(suffix);
            return self.push_line(line);
        }

        self.render_truncated = true;
        let remaining = self
            .content_limit
            .saturating_sub(self.rendered_bytes)
            .saturating_sub(separator_bytes);
        let truncated_suffix = " [line truncated]";
        let fixed_bytes = prefix.len().saturating_add(truncated_suffix.len());
        if remaining < fixed_bytes {
            return false;
        }
        let mut text_end = remaining.saturating_sub(fixed_bytes).min(text.len());
        while text_end > 0 && !text.is_char_boundary(text_end) {
            text_end -= 1;
        }
        let mut line = String::with_capacity(prefix.len() + text_end + truncated_suffix.len());
        line.push_str(&prefix);
        line.push_str(&text[..text_end]);
        line.push_str(truncated_suffix);
        let _ = self.push_line(line);
        false
    }

    fn finish(
        mut self,
        truncated_line_index: usize,
        truncated_line: String,
        render_reason_index: usize,
    ) -> String {
        if !self.render_truncated {
            return self.lines.join("\n");
        }
        if let Some(line) = self.lines.get_mut(truncated_line_index) {
            *line = truncated_line;
        }
        self.lines.insert(
            render_reason_index.min(self.lines.len()),
            "render_truncated_reason: MaxRenderedBytes".to_string(),
        );
        let mut rendered = self.lines.join("\n");
        rendered.push('\n');
        rendered.push_str(&source_output_truncation_marker());
        debug_assert!(rendered.len() <= SOURCE_TOOL_MAX_RENDERED_BYTES);
        rendered
    }
}

fn source_output_truncation_marker() -> String {
    format!("[source tool output truncated at {SOURCE_TOOL_MAX_RENDERED_BYTES} bytes]")
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
