use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::source_spec::READ_FILE_SPAN_TOOL_NAME;
use crate::tools::handlers::source_spec::SEARCH_SOURCE_TOOL_NAME;
use crate::tools::handlers::source_spec::SourceToolOptions;
use crate::tools::handlers::source_spec::create_read_file_span_tool;
use crate::tools::handlers::source_spec::create_search_source_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_file_search::source_search::ReadFileSpanOutput;
use codex_file_search::source_search::SOURCE_READ_DEFAULT_LINES;
use codex_file_search::source_search::SOURCE_SEARCH_DEFAULT_MAX_MATCHES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_FILE_BYTES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_ROOTS;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_WALK_DEPTH;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_WALK_DIRECTORIES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_WALK_ENTRIES;
use codex_file_search::source_search::SourceSearchAccumulator;
use codex_file_search::source_search::SourceSearchOptions;
use codex_file_search::source_search::SourceSearchOutput;
use codex_file_search::source_search::read_file_span_from_bytes;
use codex_file_search::source_search::should_descend_source_path;
use codex_file_search::source_search::should_scan_source_file;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::FileMetadata;
use codex_file_system::FileSystemSandboxContext;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

const SOURCE_TOOL_MAX_RENDERED_BYTES: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
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
struct ReadFileSpanArgs {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    line_count: Option<usize>,
    #[serde(default)]
    environment_id: Option<String>,
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
        Box::pin(handle_search_source(invocation))
    }
}

impl CoreToolRuntime for SearchSourceHandler {}

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
        Box::pin(handle_read_file_span(invocation))
    }
}

impl CoreToolRuntime for ReadFileSpanHandler {}

async fn handle_search_source(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "search_source received unsupported payload".to_string(),
        ));
    };
    let args: SearchSourceArgs = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse search_source arguments: {err}"))
    })?;
    let source_context = local_source_context(&invocation, args.environment_id.as_deref()).await?;
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
    let recover_explicit_root_failures = !options.roots.is_empty();
    let roots = resolve_search_roots(&source_context, &options.roots).await?;
    let mut accumulator = SourceSearchAccumulator::new(&options)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
    scan_source_roots(
        &source_context,
        &roots,
        &options,
        &mut accumulator,
        recover_explicit_root_failures,
    )
    .await?;
    let output = accumulator.finish(
        roots
            .iter()
            .map(|root| relative_source_path(&source_context, root))
            .collect::<Result<Vec<_>, _>>()?,
    );

    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        render_search_output(&output),
        Some(true),
    )))
}

async fn handle_read_file_span(
    invocation: ToolInvocation,
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
    let source_context = local_source_context(&invocation, args.environment_id.as_deref()).await?;
    let path = resolve_confined_path(&source_context, &args.path, "source file").await?;
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
    let relative_path = relative_source_path(&source_context, &path)?;
    let output = read_file_span_from_bytes(
        relative_path,
        bytes,
        args.start_line.unwrap_or(1),
        args.line_count.unwrap_or(SOURCE_READ_DEFAULT_LINES),
    )
    .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        render_read_output(&output),
        Some(true),
    )))
}

struct LocalSourceContext {
    fs: Arc<dyn ExecutorFileSystem>,
    sandbox: FileSystemSandboxContext,
    repo_root: PathUri,
    repo_root_abs: AbsolutePathBuf,
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
    let repo_root = find_repo_root(fs.as_ref(), &sandbox, &cwd).await?;
    let repo_root_abs = repo_root.to_abs_path().map_err(|err| {
        FunctionCallError::RespondToModel(format!("source repo root is not host-native: {err}"))
    })?;
    Ok(LocalSourceContext {
        fs,
        sandbox,
        repo_root,
        repo_root_abs,
    })
}

async fn find_repo_root(
    fs: &dyn ExecutorFileSystem,
    sandbox: &FileSystemSandboxContext,
    cwd: &PathUri,
) -> Result<PathUri, FunctionCallError> {
    for ancestor in cwd.ancestors() {
        let dot_git = ancestor.join(".git").map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "unable to resolve repository marker below `{}`: {err}",
                ancestor.inferred_native_path_string()
            ))
        })?;
        match fs.get_metadata(&dot_git, Some(sandbox)).await {
            Ok(metadata) if metadata.is_directory || metadata.is_file => return Ok(ancestor),
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(source_fs_error("inspect", &dot_git, err)),
        }
    }
    Ok(cwd.clone())
}

async fn resolve_search_roots(
    context: &LocalSourceContext,
    roots: &[PathBuf],
) -> Result<Vec<PathUri>, FunctionCallError> {
    if roots.len() > SOURCE_SEARCH_MAX_ROOTS {
        return Err(FunctionCallError::RespondToModel(format!(
            "too many source roots ({} provided, max {})",
            roots.len(),
            SOURCE_SEARCH_MAX_ROOTS
        )));
    }
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

async fn scan_source_root(
    context: &LocalSourceContext,
    root: &PathUri,
    options: &SourceSearchOptions,
    accumulator: &mut SourceSearchAccumulator,
) -> Result<(), FunctionCallError> {
    let metadata = context
        .fs
        .get_metadata(root, Some(&context.sandbox))
        .await
        .map_err(|err| source_fs_error("inspect", root, err))?;
    if metadata.is_file {
        return add_source_file(context, root, accumulator).await;
    }
    if !metadata.is_directory {
        return Err(FunctionCallError::RespondToModel(format!(
            "source root `{}` is neither a file nor a directory",
            root.inferred_native_path_string()
        )));
    }

    let mut queue = VecDeque::from([(root.clone(), 0usize)]);
    while let Some((directory, depth)) = queue.pop_front() {
        if accumulator.should_stop() {
            break;
        }
        if !accumulator.reserve_walk_directory(SOURCE_SEARCH_MAX_WALK_DIRECTORIES) {
            break;
        }
        let entries_result = context
            .fs
            .read_directory(&directory, Some(&context.sandbox))
            .await;
        let mut entries = if depth == 0 {
            entries_result.map_err(|err| source_fs_error("read directory", &directory, err))?
        } else {
            let Some(entries) = recover_scan_result(entries_result, accumulator) else {
                continue;
            };
            entries
        };
        entries.sort_by(|left, right| left.file_name.cmp(&right.file_name));

        for entry in entries {
            if accumulator.should_stop() {
                break;
            }
            if !accumulator.reserve_walk_entry(SOURCE_SEARCH_MAX_WALK_ENTRIES) {
                return Ok(());
            }
            let Some(child) = recover_scan_result(directory.join(&entry.file_name), accumulator)
            else {
                continue;
            };
            let Some(child_metadata) = recover_scan_result(
                context
                    .fs
                    .get_metadata(&child, Some(&context.sandbox))
                    .await,
                accumulator,
            ) else {
                continue;
            };

            if child_metadata.is_directory {
                let Some(relative) =
                    recover_scan_result(relative_source_path(context, &child), accumulator)
                else {
                    continue;
                };
                if child_metadata.is_symlink
                    || !should_descend_source_path(
                        Path::new(&relative),
                        options.include_generated,
                        options.include_vendor,
                    )
                {
                    continue;
                }
                if depth >= SOURCE_SEARCH_MAX_WALK_DEPTH {
                    accumulator.mark_walk_limit();
                    continue;
                }
                queue.push_back((child, depth.saturating_add(1)));
                continue;
            }
            if !child_metadata.is_file {
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
            ) {
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
                add_source_file(context, &canonical, accumulator).await,
                accumulator,
            );
        }
    }
    Ok(())
}

async fn scan_source_roots(
    context: &LocalSourceContext,
    roots: &[PathUri],
    options: &SourceSearchOptions,
    accumulator: &mut SourceSearchAccumulator,
    recover_root_failures: bool,
) -> Result<(), FunctionCallError> {
    for root in roots {
        if accumulator.should_stop() {
            break;
        }
        let result = scan_source_root(context, root, options, accumulator).await;
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
    if !accumulator.consider_file(Path::new(&relative), file_len) {
        return Ok(());
    }
    match read_source_file_stably(context, path, &metadata).await? {
        Some(bytes) => accumulator.add_file_bytes(Path::new(&relative), bytes),
        None => accumulator.mark_file_changed_during_read(),
    }
    Ok(())
}

async fn read_source_file_stably(
    context: &LocalSourceContext,
    path: &PathUri,
    metadata_before: &FileMetadata,
) -> Result<Option<Vec<u8>>, FunctionCallError> {
    let expected_len = usize::try_from(metadata_before.size).unwrap_or(usize::MAX);
    let bytes = match context
        .fs
        .read_file_bounded(path, SOURCE_SEARCH_MAX_FILE_BYTES, Some(&context.sandbox))
        .await
    {
        Ok(bytes) => bytes,
        Err(err) if is_changed_file_race_error(err.kind()) => return Ok(None),
        Err(err) => return Err(source_fs_error("read", path, err)),
    };
    let Some(bytes) = bytes else {
        return Ok(None);
    };
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
    matches!(
        kind,
        ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::InvalidInput
    )
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
    let rendered = render_search_output_inner(output, false);
    if rendered.len() <= SOURCE_TOOL_MAX_RENDERED_BYTES {
        return rendered;
    }
    bound_model_output(render_search_output_inner(output, true))
}

fn render_search_output_inner(output: &SourceSearchOutput, render_truncated: bool) -> String {
    let mut rendered = vec![
        "Source search evidence:".to_string(),
        format!("query: {}", output.query),
        format!(
            "coverage: files={} skipped_too_large={} skipped_non_utf8={} changed_during_read={} filesystem_errors={} bytes={} total_matches={} returned={} truncated={}",
            output.coverage.files_scanned,
            output.coverage.files_skipped_too_large,
            output.coverage.files_skipped_non_utf8,
            output.coverage.files_changed_during_read,
            output.coverage.filesystem_errors,
            output.coverage.bytes_scanned,
            output.coverage.total_matches,
            output.coverage.matches_returned,
            output.truncated || render_truncated
        ),
    ];
    if let Some(reason) = output.truncated_reason {
        rendered.push(format!("truncated_reason: {reason:?}"));
    }
    if render_truncated {
        rendered.push("render_truncated_reason: MaxRenderedBytes".to_string());
    }
    for source_match in &output.matches {
        rendered.push(String::new());
        rendered.push(format!(
            "citation: {}:{}-{} (match line {})",
            source_match.path,
            source_match.start_line,
            source_match.end_line,
            source_match.line_number
        ));
        if let Some(route) = &source_match.source_map_route {
            rendered.push(format!("source_route: {route}"));
        }
        rendered.extend(source_match.lines.iter().map(|line| {
            let suffix = if line.text_truncated {
                " [line truncated]"
            } else {
                ""
            };
            format!("{:>6} | {}{suffix}", line.line_number, line.text)
        }));
    }
    rendered.join("\n")
}

fn render_read_output(output: &ReadFileSpanOutput) -> String {
    let rendered = render_read_output_inner(output, false);
    if rendered.len() <= SOURCE_TOOL_MAX_RENDERED_BYTES {
        return rendered;
    }
    bound_model_output(render_read_output_inner(output, true))
}

fn render_read_output_inner(output: &ReadFileSpanOutput, render_truncated: bool) -> String {
    let citation = match (output.start_line, output.end_line) {
        (Some(start), Some(end)) => format!("{}:{start}-{end}", output.path),
        _ => format!("{}:<empty>", output.path),
    };
    let mut rendered = vec![
        "Source file evidence:".to_string(),
        format!("citation: {citation}"),
        format!(
            "total_lines: {} bytes_returned: {} truncated: {}",
            output.total_lines,
            output.bytes_returned,
            output.truncated || render_truncated
        ),
    ];
    if render_truncated {
        rendered.push("render_truncated_reason: MaxRenderedBytes".to_string());
    }
    if let Some(route) = &output.source_map_route {
        rendered.push(format!("source_route: {route}"));
    }
    rendered.extend(output.lines.iter().map(|line| {
        let suffix = if line.text_truncated {
            " [line truncated]"
        } else {
            ""
        };
        format!("{:>6} | {}{suffix}", line.line_number, line.text)
    }));
    rendered.join("\n")
}

fn bound_model_output(rendered: String) -> String {
    if rendered.len() <= SOURCE_TOOL_MAX_RENDERED_BYTES {
        return rendered;
    }
    let marker =
        format!("\n[source tool output truncated at {SOURCE_TOOL_MAX_RENDERED_BYTES} bytes]");
    let max_content_bytes = SOURCE_TOOL_MAX_RENDERED_BYTES.saturating_sub(marker.len());
    let mut end = max_content_bytes.min(rendered.len());
    while end > 0 && !rendered.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = rendered[..end].to_string();
    bounded.push_str(&marker);
    bounded
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
