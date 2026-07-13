use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ToolCallSource;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_exec_server::LocalFileSystem;
use codex_file_search::source_search::SourceLine;
use codex_file_search::source_search::SourceSearchCoverage;
use codex_file_search::source_search::SourceSearchMatch;
use codex_file_search::source_search::SourceTruncatedReason;
use codex_file_system::CopyOptions;
use codex_file_system::CreateDirectoryOptions;
use codex_file_system::ExecutorFileSystemFuture;
use codex_file_system::FileMetadata;
use codex_file_system::FileSystemReadStream;
use codex_file_system::ReadDirectoryEntry;
use codex_file_system::RemoveOptions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use core_test_support::TempDirExt;
use serde_json::json;
use std::io;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InjectedSourceFailure {
    Metadata,
    ReadDirectory,
    Canonicalize,
    Read,
}

struct FailingSourceFileSystem {
    inner: LocalFileSystem,
    target: AbsolutePathBuf,
    failure: InjectedSourceFailure,
}

impl FailingSourceFileSystem {
    fn targets(&self, path: &PathUri, failure: InjectedSourceFailure) -> bool {
        self.failure == failure && path.to_abs_path().is_ok_and(|path| path == self.target)
    }
}

impl ExecutorFileSystem for FailingSourceFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async move {
            if self.targets(path, InjectedSourceFailure::Canonicalize) {
                return Err(io::Error::other("injected canonicalize failure"));
            }
            self.inner.canonicalize(path, sandbox).await
        })
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(async move {
            if self.targets(path, InjectedSourceFailure::Read) {
                return Err(io::Error::other("injected read failure"));
            }
            self.inner.read_file(path, sandbox).await
        })
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        self.inner.read_file_stream(path, sandbox)
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner.write_file(path, contents, sandbox)
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner.create_directory(path, options, sandbox)
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async move {
            if self.targets(path, InjectedSourceFailure::Metadata) {
                return Err(io::Error::other("injected metadata failure"));
            }
            self.inner.get_metadata(path, sandbox).await
        })
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(async move {
            if self.targets(path, InjectedSourceFailure::ReadDirectory) {
                return Err(io::Error::other("injected directory read failure"));
            }
            self.inner.read_directory(path, sandbox).await
        })
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner.remove(path, options, sandbox)
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner
            .copy(source_path, destination_path, options, sandbox)
    }
}

fn replace_primary_environment_cwd(turn: &mut crate::TurnContext, cwd: AbsolutePathBuf) {
    let current = turn
        .environments
        .turn_environments
        .first()
        .cloned()
        .expect("default local turn environment");
    turn.environments.turn_environments[0] = TurnEnvironment::new(
        current.environment_id,
        current.environment,
        PathUri::from_abs_path(&cwd),
        current.shell,
    );
}

fn sample_search_output(text: String) -> SourceSearchOutput {
    SourceSearchOutput {
        query: "needle".to_string(),
        roots: vec![".".to_string()],
        truncated: false,
        truncated_reason: None,
        coverage: SourceSearchCoverage {
            files_scanned: 1,
            files_skipped_too_large: 0,
            files_skipped_non_utf8: 0,
            filesystem_errors: 0,
            bytes_scanned: 10,
            result_bytes: text.len(),
            total_matches: 1,
            matches_returned: 1,
            max_matches: 100,
            max_files: 2_000,
            max_bytes: 16 * 1024 * 1024,
            max_file_bytes: 2 * 1024 * 1024,
            max_result_bytes: 512 * 1024,
        },
        matches: vec![SourceSearchMatch {
            path: "src/lib.rs".to_string(),
            source_map_route: Some("src".to_string()),
            line_number: 8,
            start_line: 7,
            end_line: 9,
            lines: vec![SourceLine {
                line_number: 8,
                text,
                text_truncated: false,
            }],
        }],
    }
}

#[test]
fn search_render_includes_explicit_line_span_evidence() {
    let output = sample_search_output("needle".to_string());

    let rendered = render_search_output(&output);
    assert!(rendered.contains("citation: src/lib.rs:7-9 (match line 8)"));
    assert!(rendered.contains("     8 | needle"));
}

#[test]
fn search_render_is_capped_below_model_context_limit() {
    let output = sample_search_output("x".repeat(SOURCE_TOOL_MAX_RENDERED_BYTES * 2));

    let rendered = render_search_output(&output);

    assert!(rendered.len() <= SOURCE_TOOL_MAX_RENDERED_BYTES);
    assert!(rendered.contains("[source tool output truncated at 8192 bytes]"));
    assert!(rendered.contains("truncated=true"));
    assert!(rendered.contains("render_truncated_reason: MaxRenderedBytes"));
    assert!(!rendered.contains("truncated=false"));
}

#[test]
fn read_render_is_capped_below_model_context_limit() {
    let output = ReadFileSpanOutput {
        path: "src/lib.rs".to_string(),
        source_map_route: Some("src".to_string()),
        requested_start_line: 1,
        requested_line_count: 1,
        start_line: Some(1),
        end_line: Some(1),
        total_lines: 1,
        bytes_returned: SOURCE_TOOL_MAX_RENDERED_BYTES * 2,
        truncated: false,
        lines: vec![SourceLine {
            line_number: 1,
            text: "x".repeat(SOURCE_TOOL_MAX_RENDERED_BYTES * 2),
            text_truncated: false,
        }],
    };

    let rendered = render_read_output(&output);

    assert!(rendered.len() <= SOURCE_TOOL_MAX_RENDERED_BYTES);
    assert!(rendered.contains("[source tool output truncated at 8192 bytes]"));
    assert!(rendered.contains("truncated: true"));
    assert!(rendered.contains("render_truncated_reason: MaxRenderedBytes"));
    assert!(!rendered.contains("truncated: false"));
}

#[tokio::test]
async fn source_scan_preserves_partial_results_across_filesystem_failures() {
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let repo_root_abs = source_dir.abs();
    let bad_file = repo_root_abs.join("b_bad.rs");
    let bad_directory = repo_root_abs.join("b_bad_dir");
    let later_directory = repo_root_abs.join("d_good_dir");
    std::fs::write(repo_root_abs.join("a_good.rs").as_path(), "needle a\n")
        .expect("write first good source");
    std::fs::write(bad_file.as_path(), "needle bad file\n").expect("write bad source");
    std::fs::write(repo_root_abs.join("c_good.rs").as_path(), "needle c\n")
        .expect("write later good source");
    std::fs::create_dir(bad_directory.as_path()).expect("create bad directory");
    std::fs::write(bad_directory.join("hidden.rs").as_path(), "needle hidden\n")
        .expect("write source in bad directory");
    std::fs::create_dir(later_directory.as_path()).expect("create later directory");
    std::fs::write(
        later_directory.join("nested.rs").as_path(),
        "needle nested\n",
    )
    .expect("write later nested source");

    let root = PathUri::from_abs_path(&repo_root_abs);
    let cases = [
        (
            InjectedSourceFailure::Metadata,
            bad_file.clone(),
            "b_bad.rs",
        ),
        (
            InjectedSourceFailure::Canonicalize,
            bad_file.clone(),
            "b_bad.rs",
        ),
        (InjectedSourceFailure::Read, bad_file, "b_bad.rs"),
        (
            InjectedSourceFailure::ReadDirectory,
            bad_directory,
            "b_bad_dir/hidden.rs",
        ),
    ];

    for (failure, target, omitted_path) in cases {
        let context = LocalSourceContext {
            fs: Arc::new(FailingSourceFileSystem {
                inner: LocalFileSystem::unsandboxed(),
                target,
                failure,
            }),
            sandbox: FileSystemSandboxContext::from_permission_profile(PermissionProfile::Disabled),
            repo_root: root.clone(),
            repo_root_abs: repo_root_abs.clone(),
        };
        let options = SourceSearchOptions::new(PathBuf::new(), "needle".to_string());
        let mut accumulator =
            SourceSearchAccumulator::new(&options).expect("create source accumulator");

        scan_source_root(&context, &root, &options, &mut accumulator)
            .await
            .expect("recoverable source scan");
        let output = accumulator.finish(vec![".".to_string()]);
        let paths = output
            .matches
            .iter()
            .map(|source_match| source_match.path.as_str())
            .collect::<Vec<_>>();

        assert!(paths.contains(&"a_good.rs"), "{failure:?}: {paths:?}");
        assert!(paths.contains(&"c_good.rs"), "{failure:?}: {paths:?}");
        assert!(
            paths.contains(&"d_good_dir/nested.rs"),
            "{failure:?}: {paths:?}"
        );
        assert!(!paths.contains(&omitted_path), "{failure:?}: {paths:?}");
        assert_eq!(output.coverage.filesystem_errors, 1, "{failure:?}");
        assert_eq!(
            output.truncated_reason,
            Some(SourceTruncatedReason::FilesystemErrors),
            "{failure:?}"
        );
    }
}

#[tokio::test]
async fn source_scan_rejects_a_root_directory_read_failure() {
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let repo_root_abs = source_dir.abs();
    let root = PathUri::from_abs_path(&repo_root_abs);
    let context = LocalSourceContext {
        fs: Arc::new(FailingSourceFileSystem {
            inner: LocalFileSystem::unsandboxed(),
            target: repo_root_abs.clone(),
            failure: InjectedSourceFailure::ReadDirectory,
        }),
        sandbox: FileSystemSandboxContext::from_permission_profile(PermissionProfile::Disabled),
        repo_root: root.clone(),
        repo_root_abs,
    };
    let options = SourceSearchOptions::new(PathBuf::new(), "needle".to_string());
    let mut accumulator = SourceSearchAccumulator::new(&options).expect("source accumulator");

    let error = scan_source_root(&context, &root, &options, &mut accumulator)
        .await
        .expect_err("root directory failure must be terminal");

    assert!(error.to_string().contains("read directory"));
}

#[tokio::test]
async fn explicit_source_roots_preserve_partial_results_when_one_root_inspect_or_read_fails() {
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let repo_root_abs = source_dir.abs();
    let first_root = repo_root_abs.join("a_good");
    let bad_root = repo_root_abs.join("b_bad");
    let later_root = repo_root_abs.join("c_good");
    for root in [&first_root, &bad_root, &later_root] {
        std::fs::create_dir(root.as_path()).expect("create explicit root");
    }
    std::fs::write(first_root.join("first.rs").as_path(), "needle first\n")
        .expect("write first root source");
    std::fs::write(bad_root.join("hidden.rs").as_path(), "needle hidden\n")
        .expect("write bad root source");
    std::fs::write(later_root.join("later.rs").as_path(), "needle later\n")
        .expect("write later root source");

    let roots = [
        PathUri::from_abs_path(&first_root),
        PathUri::from_abs_path(&bad_root),
        PathUri::from_abs_path(&later_root),
    ];

    for failure in [
        InjectedSourceFailure::Metadata,
        InjectedSourceFailure::ReadDirectory,
    ] {
        let context = LocalSourceContext {
            fs: Arc::new(FailingSourceFileSystem {
                inner: LocalFileSystem::unsandboxed(),
                target: bad_root.clone(),
                failure,
            }),
            sandbox: FileSystemSandboxContext::from_permission_profile(PermissionProfile::Disabled),
            repo_root: PathUri::from_abs_path(&repo_root_abs),
            repo_root_abs: repo_root_abs.clone(),
        };
        let options = SourceSearchOptions::new(PathBuf::new(), "needle".to_string());
        let mut accumulator = SourceSearchAccumulator::new(&options).expect("source accumulator");

        scan_source_roots(
            &context,
            &roots,
            &options,
            &mut accumulator,
            /*recover_root_failures*/ true,
        )
        .await
        .expect("explicit root failure is recoverable");
        let output = accumulator.finish(vec![
            "a_good".to_string(),
            "b_bad".to_string(),
            "c_good".to_string(),
        ]);
        let paths = output
            .matches
            .iter()
            .map(|source_match| source_match.path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            vec!["a_good/first.rs", "c_good/later.rs"],
            "{failure:?}"
        );
        assert_eq!(output.coverage.filesystem_errors, 1, "{failure:?}");
        assert_eq!(
            output.truncated_reason,
            Some(SourceTruncatedReason::FilesystemErrors),
            "{failure:?}"
        );
    }
}

#[tokio::test]
async fn search_handler_passes_sandbox_context_to_filesystem_operations() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let source_cwd = source_dir.abs();
    replace_primary_environment_cwd(&mut turn, source_cwd);
    turn.permission_profile = PermissionProfile::read_only();
    let turn = Arc::new(turn);

    let result = SearchSourceHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-search-source".to_string(),
            tool_name: ToolName::plain(SEARCH_SOURCE_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({ "query": "needle" }).to_string(),
            },
        })
        .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected sandboxed filesystem error");
    };
    assert!(
        message.contains("sandboxed filesystem operations require configured runtime paths"),
        "{message}"
    );
}

#[tokio::test]
async fn search_handler_reads_through_selected_local_filesystem() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let source_cwd = source_dir.abs();
    std::fs::create_dir(source_cwd.join(".git").as_path()).expect("create git marker");
    std::fs::create_dir(source_cwd.join("src").as_path()).expect("create src");
    std::fs::write(
        source_cwd.join("src/lib.rs").as_path(),
        "before\nneedle\nafter\n",
    )
    .expect("write source");
    replace_primary_environment_cwd(&mut turn, source_cwd);
    turn.permission_profile = PermissionProfile::Disabled;
    let turn = Arc::new(turn);
    let payload = ToolPayload::Function {
        arguments: json!({ "query": "needle", "paths": ["src"] }).to_string(),
    };

    let output = SearchSourceHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-search-source-success".to_string(),
            tool_name: ToolName::plain(SEARCH_SOURCE_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .expect("source search should succeed");

    let ResponseInputItem::FunctionCallOutput { output, .. } =
        output.to_response_item("call-search-source-success", &payload)
    else {
        panic!("expected function call output");
    };
    let text = output.body.to_text().expect("text output");
    assert!(text.contains("citation: src/lib.rs:2-2 (match line 2)"));
    assert!(text.contains("     2 | needle"));
}
