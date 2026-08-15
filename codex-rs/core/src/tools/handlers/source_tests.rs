use super::*;
use crate::git_workspace::GitWorkspaceCache;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::command_execution::record_finalized_workspace_mutation;
use crate::tools::context::ToolCallSource;
use crate::tools::known_delta_store;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_agent_task_store::WorkspaceMutationResult;
use codex_core_skills::HostSkillsSnapshot;
use codex_core_skills::SkillLoadOutcome;
use codex_core_skills::SkillMetadata;
use codex_core_skills::skill_catalog_id;
use codex_exec_server::Environment;
use codex_exec_server::LocalFileSystem;
use codex_features::Feature;
use codex_file_search::source_search::SOURCE_READ_MAX_LINES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_CONTEXT_LINES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_MATCHES;
use codex_file_search::source_search::SourceLine;
use codex_file_search::source_search::SourceSearchCoverage;
use codex_file_search::source_search::SourceSearchDiagnostics;
use codex_file_search::source_search::SourceSearchHydratedSpan;
use codex_file_search::source_search::SourceSearchHydrationIssue;
use codex_file_search::source_search::SourceSearchHydrationIssueReason;
use codex_file_search::source_search::SourceSearchHydrationPacket;
use codex_file_search::source_search::SourceSearchHydrationPacketSpan;
use codex_file_search::source_search::SourceSearchHydrationSelection;
use codex_file_search::source_search::SourceSearchHydrationStatus;
use codex_file_search::source_search::SourceSearchMatch;
use codex_file_search::source_search::SourceTruncatedReason;
use codex_file_system::CopyOptions;
use codex_file_system::CreateDirectoryOptions;
use codex_file_system::ExecutorFileSystemFuture;
use codex_file_system::FileMetadata;
use codex_file_system::FileSystemReadStream;
use codex_file_system::ReadDirectoryEntry;
use codex_file_system::ReadDirectoryOutcome;
use codex_file_system::RemoveOptions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::SkillScope;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use core_test_support::TempDirExt;
use serde_json::json;
use std::fs::FileTimes;
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
        Box::pin(async move {
            if self.targets(path, InjectedSourceFailure::Read) {
                return Err(io::Error::other("injected read failure"));
            }
            self.inner.read_file_stream(path, sandbox).await
        })
    }

    fn read_file_bounded_confined<'a>(
        &'a self,
        path: &'a PathUri,
        root: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            if self.targets(path, InjectedSourceFailure::Read) {
                return Err(io::Error::other("injected read failure"));
            }
            self.inner
                .read_file_bounded_confined(path, root, max_bytes, sandbox)
                .await
        })
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

    fn read_directory_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_entries: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome> {
        Box::pin(async move {
            if self.targets(path, InjectedSourceFailure::ReadDirectory) {
                return Err(io::Error::other("injected directory read failure"));
            }
            self.inner
                .read_directory_bounded(path, max_entries, sandbox)
                .await
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
        coverage_complete: true,
        coverage_note: None,
        coverage: SourceSearchCoverage {
            walked_entries: 1,
            ignored_entries: 0,
            files_scanned: 1,
            files_skipped_too_large: 0,
            files_skipped_non_utf8: 0,
            files_changed_during_read: 0,
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
            index_complete: true,
            context_complete: true,
            indexed_matches: 1,
            omitted_contexts: 0,
            result_cap_reached: false,
        },
        matches: vec![SourceSearchMatch {
            id: "match:fixture".to_string(),
            file_id: "file:fixture".to_string(),
            path: "src/lib.rs".to_string(),
            source_revision: "revision".to_string(),
            source_map_route: Some("src".to_string()),
            line_number: 8,
            matched_content: "needle".to_string(),
            start_line: 7,
            end_line: 9,
            context_complete: true,
            lines: vec![SourceLine {
                line_number: 8,
                text,
                text_truncated: false,
            }],
        }],
        hydration_status: SourceSearchHydrationStatus::SkippedObservationUnavailable,
        hydrated_span: None,
        hydration_packet: None,
        diagnostics: SourceSearchDiagnostics::default(),
    }
}

fn sample_packet_search_output() -> SourceSearchOutput {
    let mut output = sample_search_output("ordinary hydrated context".to_string());
    let mut second = output.matches[0].clone();
    second.id = "match:omitted".to_string();
    second.file_id = "file:omitted".to_string();
    second.path = "src/other.rs".to_string();
    second.source_revision = "other-revision".to_string();
    second.line_number = 4;
    second.start_line = 3;
    second.end_line = 5;
    second.matched_content = "needle omitted".to_string();
    second.lines = vec![SourceLine {
        line_number: 4,
        text: "unhydrated exact context".to_string(),
        text_truncated: false,
    }];
    output.matches.push(second);
    output.coverage.total_matches = 2;
    output.coverage.matches_returned = 2;
    output.coverage.indexed_matches = 2;
    output.hydration_status = SourceSearchHydrationStatus::PartiallyHydratedBoundedPacket;
    let exact_content = "fn owner() {\n    let needle = true;\n}\n".to_string();
    let span_content_hash = format!("{:x}", Sha256::digest(exact_content.as_bytes()));
    output.hydration_packet = Some(SourceSearchHydrationPacket {
        schema_version: 1,
        observation_set_id: "observation-set".to_string(),
        exact_content_byte_limit: 5 * 1024,
        exact_content_bytes: exact_content.len(),
        spans: vec![SourceSearchHydrationPacketSpan {
            id: "source-hydration:fixture".to_string(),
            match_ids: vec!["match:fixture".to_string()],
            path: "src/lib.rs".to_string(),
            requested_start_line: 7,
            requested_end_line: 9,
            start_line: 7,
            end_line: 9,
            file_content_hash: "revision".to_string(),
            span_content_hash,
            selection: SourceSearchHydrationSelection::AuthoritativeDefinition,
            truncated: false,
            exact_content,
        }],
        issues: vec![SourceSearchHydrationIssue {
            match_id: "match:omitted".to_string(),
            reason: SourceSearchHydrationIssueReason::ByteCap,
        }],
    });
    output
}

fn sample_unique_hydrated_search_output() -> SourceSearchOutput {
    let mut output = sample_search_output("ordinary match context".to_string());
    let exact_content = [
        "fn helper() {}",
        "",
        "fn owner() {",
        "    let before = true;",
        "    let needle = true;",
        "    let after = true;",
        "}",
    ]
    .join("\n");
    let observation = read_file_span_from_bytes(
        "src/lib.rs".to_string(),
        exact_content.as_bytes().to_vec(),
        3,
        5,
    )
    .expect("unique hydration span");
    output.hydration_status = SourceSearchHydrationStatus::HydratedAuthoritativeDefinition;
    output.hydrated_span = Some(SourceSearchHydratedSpan {
        content_hash: format!("{:x}", Sha256::digest(exact_content.as_bytes())),
        observation,
    });
    output
}

#[test]
fn search_render_includes_explicit_line_span_evidence() {
    let output = sample_search_output("needle".to_string());

    let rendered = render_search_output(&output);
    assert!(rendered.contains("citation: src/lib.rs:7-9 (match line 8)"));
    assert!(rendered.contains("     8 | needle"));
}

#[test]
fn packet_search_render_includes_exact_spans_and_omissions_without_duplicate_context() {
    let output = sample_packet_search_output();

    let rendered = render_search_output(&output);

    assert!(rendered.len() <= SOURCE_TOOL_MAX_RENDERED_BYTES);
    assert!(rendered.contains("hydration_packet: schema=1 observation_set_id=observation-set"));
    assert!(rendered.contains("hydrated_citation: src/lib.rs:7-9 requested=7-9"));
    assert!(rendered.contains("span_content_hash:"));
    assert!(rendered.contains("     8 |     let needle = true;"));
    assert!(rendered.contains("hydration_issue: match_id=match:omitted reason=ByteCap"));
    assert!(rendered.contains("unhydrated exact context"));
    assert!(!rendered.contains("ordinary hydrated context"));
    let last_identity = rendered
        .rfind("match_identity:")
        .expect("packet match identities");
    let first_span = rendered
        .find("hydrated_span_id:")
        .expect("hydrated packet span");
    assert!(last_identity < first_span);
}

#[tokio::test]
async fn packet_search_projection_and_signal_preserve_exact_hydration() {
    let output = sample_packet_search_output();
    let expected_packet = output.hydration_packet.clone().expect("packet");
    let (_session, turn) = make_session_and_context().await;

    let tool_output = search_function_output(&output, false, false, None, &turn.turn_timing_state);
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let projection = tool_output
        .projection_metadata()
        .expect("search projection");
    let hydration_fragment = projection
        .fragments
        .iter()
        .find(|fragment| fragment.id.as_deref() == Some("source-hydration:fixture"))
        .expect("hydration fragment");
    assert_eq!(
        hydration_fragment.text,
        expected_packet.spans[0].exact_content
    );
    assert_eq!(
        projection.predetermined_json_pointers,
        vec![ToolOutputProjectionJsonPointer {
            id: "source-hydration:fixture".to_string(),
            pointer: "/hydration_packet/spans/0".to_string(),
        }]
    );
    assert!(
        projection
            .predetermined_json_pointers
            .iter()
            .all(|selector| !selector.pointer.starts_with("/matches/"))
    );

    let canonical = tool_output
        .canonical_result(&payload)
        .expect("canonical search result");
    let canonical: SourceSearchOutput =
        serde_json::from_slice(&canonical.bytes).expect("decode canonical search output");
    assert_eq!(canonical.hydration_packet, Some(expected_packet));

    let code_mode: SourceSearchOutput =
        serde_json::from_value(tool_output.code_mode_result(&payload))
            .expect("decode native code-mode search output");
    assert_eq!(code_mode.hydration_packet, canonical.hydration_packet);

    let signal = tool_output
        .sampling_request_signal()
        .expect("sampling signal");
    assert_eq!(
        signal
            .get("match_count")
            .and_then(serde_json::Value::as_u64),
        Some(2)
    );
    assert_eq!(
        signal.get("match_paths"),
        Some(&serde_json::json!(["src/lib.rs", "src/other.rs"]))
    );
    assert_eq!(
        signal
            .get("hydration_omission_count")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );
    for unused in [
        "query",
        "roots",
        "match_ids",
        "hydrated_match_ids",
        "hydration_observation_set_id",
        "hydration_span_count",
        "hydration_exact_content_bytes",
        "hydration_issues",
        "source_disposition",
    ] {
        assert!(
            signal.get(unused).is_none(),
            "unexpected private field {unused}"
        );
    }
}

#[tokio::test]
async fn unique_search_projection_preserves_exact_hydration() {
    let output = sample_unique_hydrated_search_output();
    let expected = output
        .hydrated_span
        .as_ref()
        .expect("unique hydration")
        .observation
        .exact_content
        .clone();
    let (_session, turn) = make_session_and_context().await;

    let tool_output = search_function_output(&output, false, false, None, &turn.turn_timing_state);
    let projection = tool_output
        .projection_metadata()
        .expect("search projection");
    let hydration_fragment = projection
        .fragments
        .iter()
        .find(|fragment| fragment.id.as_deref() == Some("match:fixture:hydrated-span"))
        .expect("unique hydration fragment");

    assert_eq!(hydration_fragment.text, expected);
    assert_eq!(
        hydration_fragment.kind,
        ToolOutputProjectionFragmentKind::CitationOrExactSpan
    );
    assert_eq!(
        projection.predetermined_json_pointers,
        vec![ToolOutputProjectionJsonPointer {
            id: "match:fixture:hydrated-span".to_string(),
            pointer: "/hydrated_span".to_string(),
        }]
    );
}

#[tokio::test]
async fn search_projection_owner_recovery_only_adds_issue_for_policy_selected_span() {
    let mut output = sample_packet_search_output();
    output
        .hydration_packet
        .as_mut()
        .expect("hydration packet")
        .issues[0]
        .match_id = "match:fixture".to_string();
    let (_session, turn) = make_session_and_context().await;

    let projection = search_function_output(&output, false, false, None, &turn.turn_timing_state)
        .projection_metadata()
        .expect("search projection");

    assert_eq!(
        projection.predetermined_json_pointers,
        vec![
            ToolOutputProjectionJsonPointer {
                id: "source-hydration:fixture".to_string(),
                pointer: "/hydration_packet/spans/0".to_string(),
            },
            ToolOutputProjectionJsonPointer {
                id: "source-hydration-issue:match:fixture".to_string(),
                pointer: "/hydration_packet/issues/0".to_string(),
            },
        ]
    );
    assert!(
        projection
            .predetermined_json_pointers
            .iter()
            .all(|selector| !selector.pointer.starts_with("/matches/"))
    );
}

#[tokio::test]
async fn source_read_canonicalizes_exact_content_before_bounded_rendering() {
    let content = (1..=126)
        .map(|line| format!("line {line}\r\n"))
        .collect::<String>();
    let output = read_file_span_from_bytes(
        "src/fixture.rs".to_string(),
        content.as_bytes().to_vec(),
        1,
        126,
    )
    .expect("source fixture");
    let (_session, turn) = make_session_and_context().await;
    let tool_output = source_read_tool_output(
        output,
        "bounded renderer output".to_string(),
        serde_json::json!({ "kind": "source_evidence" }),
        None,
        &turn.turn_timing_state,
    );
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };

    let canonical = tool_output
        .canonical_result(&payload)
        .expect("canonical source result");
    assert_eq!(canonical.bytes, content.as_bytes());
    assert_eq!(
        canonical.sha256,
        format!("{:x}", Sha256::digest(content.as_bytes()))
    );
    let projection = tool_output
        .projection_metadata()
        .expect("typed source projection");
    assert_eq!(projection.fragments.len(), 4);
    assert!(projection.fragments.iter().all(|fragment| {
        fragment
            .id
            .as_deref()
            .is_some_and(|id| id.starts_with("src:"))
    }));
    assert_eq!(projection.predetermined_ranges.len(), 4);
    assert_eq!(
        projection
            .predetermined_ranges
            .iter()
            .map(|range| (range.start_line, range.end_line))
            .collect::<Vec<_>>(),
        vec![(1, 40), (41, 80), (81, 120), (121, 126)]
    );
    assert!(
        projection
            .predetermined_ranges
            .iter()
            .zip(&projection.fragments)
            .all(|(range, fragment)| fragment.id.as_deref() == Some(range.id.as_str()))
    );
}

#[tokio::test]
async fn source_predetermined_ranges_are_relative_to_the_requested_artifact() {
    let content = (1..=126)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    let output =
        read_file_span_from_bytes("src/fixture.rs".to_string(), content.into_bytes(), 41, 86)
            .expect("source fixture");
    let (_session, turn) = make_session_and_context().await;
    let tool_output = source_read_tool_output(
        output,
        "bounded renderer output".to_string(),
        serde_json::json!({ "kind": "source_evidence" }),
        None,
        &turn.turn_timing_state,
    );
    let projection = tool_output
        .projection_metadata()
        .expect("typed source projection");

    assert_eq!(
        projection
            .predetermined_ranges
            .iter()
            .map(|range| (range.start_line, range.end_line))
            .collect::<Vec<_>>(),
        vec![(1, 40), (41, 80), (81, 86)]
    );
    assert!(projection.predetermined_ranges[0].id.ends_with(":L41-L80"));
}

#[tokio::test]
async fn source_coverage_trim_keeps_canonical_and_projection_on_missing_ranges() {
    let content = (1..=8)
        .map(|line| format!("line {line}\r\n"))
        .collect::<String>();
    let mut output =
        read_file_span_from_bytes("src/fixture.rs".to_string(), content.into_bytes(), 1, 8)
            .expect("source fixture");
    retain_read_file_span_intervals(&mut output, &[(2, 2), (5, 6)]);
    let (_session, turn) = make_session_and_context().await;
    let tool_output = source_read_tool_output(
        output,
        "bounded renderer output".to_string(),
        serde_json::json!({ "kind": "source_evidence" }),
        None,
        &turn.turn_timing_state,
    );
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };

    let canonical = tool_output
        .canonical_result(&payload)
        .expect("canonical source result");
    assert_eq!(canonical.bytes, b"line 2\r\nline 5\r\nline 6\r\n");
    let projection = tool_output
        .projection_metadata()
        .expect("typed source projection");
    assert_eq!(
        projection
            .predetermined_ranges
            .iter()
            .map(|range| (range.start_line, range.end_line))
            .collect::<Vec<_>>(),
        vec![(1, 1), (2, 3)]
    );
}

#[test]
fn capped_search_render_discloses_incomplete_coverage() {
    let mut output = sample_search_output("needle".to_string());
    output.truncated = true;
    output.truncated_reason = Some(SourceTruncatedReason::WalkLimit);
    output.coverage_complete = false;
    output.coverage_note = Some(
        "No matches were found in the scanned portion of the repository. Narrow with paths or use locate_task."
            .to_string(),
    );
    output.matches.clear();
    output.coverage.matches_returned = 0;

    let rendered = render_search_output(&output);

    assert!(rendered.contains("coverage: complete=false"));
    assert!(rendered.contains("No matches were found in the scanned portion"));
    assert!(rendered.contains("paths or use locate_task"));
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
    assert!(rendered.contains("citation: src/lib.rs:7-9 (match line 8)"));
    assert!(
        rendered
            .lines()
            .any(|line| line.starts_with("     8 | ") && line.ends_with(" [line truncated]"))
    );
    assert!(rendered.ends_with("[source tool output truncated at 8192 bytes]"));
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
        full_file_sha256: String::new(),
        requested_content_sha256: String::new(),
        requested_bytes: SOURCE_TOOL_MAX_RENDERED_BYTES * 2,
        exact_content: String::new(),
        chunks: Vec::new(),
    };

    let rendered = render_read_output(&output);

    assert!(rendered.len() <= SOURCE_TOOL_MAX_RENDERED_BYTES);
    assert!(rendered.contains("[source tool output truncated at 8192 bytes]"));
    assert!(rendered.contains("truncated: true"));
    assert!(rendered.contains("render_truncated_reason: MaxRenderedBytes"));
    assert!(!rendered.contains("truncated: false"));
    assert!(rendered.contains("citation: src/lib.rs:1-1"));
    assert!(
        rendered
            .lines()
            .any(|line| line.starts_with("     1 | ") && line.ends_with(" [line truncated]"))
    );
    assert!(rendered.ends_with("[source tool output truncated at 8192 bytes]"));
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
            is_git_repository: false,
            environment_id: "local".to_string(),
        };
        let options = SourceSearchOptions::new(PathBuf::new(), "needle".to_string());
        let mut accumulator =
            SourceSearchAccumulator::new(&options).expect("create source accumulator");
        let mut observed_entries = BTreeMap::new();
        let ignore_matcher = SourceIgnoreMatcher::new_preloaded(None);

        scan_source_root(
            &context,
            &root,
            &options,
            &ignore_matcher,
            &mut accumulator,
            &mut observed_entries,
        )
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
        repo_root_abs: repo_root_abs.clone(),
        is_git_repository: false,
        environment_id: "local".to_string(),
    };
    let options = SourceSearchOptions::new(PathBuf::new(), "needle".to_string());
    let mut accumulator = SourceSearchAccumulator::new(&options).expect("source accumulator");
    let mut observed_entries = BTreeMap::new();
    let ignore_matcher = SourceIgnoreMatcher::new_preloaded(None);

    let error = scan_source_root(
        &context,
        &root,
        &options,
        &ignore_matcher,
        &mut accumulator,
        &mut observed_entries,
    )
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
            is_git_repository: false,
            environment_id: "local".to_string(),
        };
        let options = SourceSearchOptions::new(PathBuf::new(), "needle".to_string());
        let mut accumulator = SourceSearchAccumulator::new(&options).expect("source accumulator");
        let mut observed_entries = BTreeMap::new();
        let ignore_matcher = SourceIgnoreMatcher::new_preloaded(None);

        scan_source_roots(
            &context,
            &roots,
            &options,
            &ignore_matcher,
            &mut accumulator,
            &mut observed_entries,
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
            turn: Arc::clone(&turn),
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
async fn receipt_reuse_search_handler_reads_through_selected_local_filesystem() {
    let (session, mut turn) = make_session_and_context().await;
    assert!(
        session.services.state_db.is_none(),
        "this regression requires lazy durable-state initialization"
    );
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
    let session = Arc::new(session);

    let (output, first_reads) =
        test_observation::observe(SearchSourceHandler::new(false).handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn: Arc::clone(&turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-search-source-success".to_string(),
            tool_name: ToolName::plain(SEARCH_SOURCE_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        }))
        .await;
    let output = output.expect("source search should succeed");

    let ResponseInputItem::FunctionCallOutput { output, .. } =
        output.to_response_item("call-search-source-success", &payload)
    else {
        panic!("expected function call output");
    };
    let text = output.body.to_text().expect("text output");
    assert!(text.contains("citation: src/lib.rs:2-2 (match line 2)"));
    assert!(text.contains("     2 | needle"));
    let coordinator = session.services.agent_control.task_coordinator();
    let store = coordinator
        .store()
        .expect("source read lazily initializes durable coordination");
    let actor_id = format!("root:{}", session.services.agent_control.session_id());
    let manifest = store
        .supporting_read_manifest(source_dir.path(), actor_id, vec!["src/lib.rs".to_string()])
        .await
        .expect("source read persists its supporting manifest");
    assert_eq!(manifest.len(), 1);
    assert_eq!(manifest[0].path, "src/lib.rs");
    assert!(manifest[0].existed);
    assert!(manifest[0].content_hash.is_some());

    let (replayed, replay_reads) =
        test_observation::observe(SearchSourceHandler::new(false).handle(ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-search-source-replay".to_string(),
            tool_name: ToolName::plain(SEARCH_SOURCE_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload,
        }))
        .await;
    let replayed = replayed.expect("identical source search should replay");
    assert_eq!(
        replayed
            .sampling_request_signal()
            .and_then(|signal| signal.get("source_disposition").cloned()),
        Some(json!("exact_replay")),
    );
    assert_eq!(
        replay_reads.successful_content_reads.saturating_add(1),
        first_reads.successful_content_reads,
        "the replay may recompute the exact scope identity but must not rescan matched files",
    );
}

#[tokio::test]
async fn receipt_reuse_bounded_source_reads_emit_only_partial_missing_lines() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let source_cwd = source_dir.abs();
    std::fs::create_dir(source_cwd.join(".git").as_path()).expect("create git marker");
    std::fs::create_dir(source_cwd.join("src").as_path()).expect("create src");
    std::fs::write(
        source_cwd.join("src/lib.rs").as_path(),
        "one\ntwo\nthree\nfour\nfive\n",
    )
    .expect("write source");
    replace_primary_environment_cwd(&mut turn, source_cwd);
    turn.permission_profile = PermissionProfile::Disabled;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));

    let first_payload = ToolPayload::Function {
        arguments: json!({
            "path": "src/lib.rs",
            "start_line": 1,
            "line_count": 3
        })
        .to_string(),
    };
    let (first, first_reads) =
        test_observation::observe(ReadFileSpanHandler::new(false).handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn: Arc::clone(&turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::clone(&tracker),
            call_id: "source-coverage-first".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: first_payload,
        }))
        .await;
    let first = first.expect("first read");
    assert_eq!(first_reads.successful_content_reads, 1);
    assert!(first.deterministic_continuation_receipts().is_empty());

    let partial_payload = ToolPayload::Function {
        arguments: json!({
            "path": "src/lib.rs",
            "start_line": 2,
            "line_count": 4
        })
        .to_string(),
    };
    let (partial, partial_reads) =
        test_observation::observe(ReadFileSpanHandler::new(false).handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn: Arc::clone(&turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::clone(&tracker),
            call_id: "source-coverage-partial".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: partial_payload.clone(),
        }))
        .await;
    let partial = partial.expect("partial read");
    assert_eq!(
        partial_reads.successful_content_reads, 0,
        "partial overlap must be served from the freshness-verified replay artifact",
    );
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        partial.to_response_item("source-coverage-partial", &partial_payload)
    else {
        panic!("expected partial function output");
    };
    let partial_text = output.body.to_text().expect("partial text");
    assert!(
        partial_text.contains("reused_intervals: 2-3"),
        "{partial_text}"
    );
    assert!(!partial_text.contains("     2 | two"), "{partial_text}");
    assert!(!partial_text.contains("     3 | three"), "{partial_text}");
    assert!(partial_text.contains("     4 | four"), "{partial_text}");
    assert!(partial_text.contains("     5 | five"), "{partial_text}");
    let partial_receipts = partial.deterministic_continuation_receipts();
    assert_eq!(partial_receipts.len(), 1);
    assert_eq!(
        partial_receipts[0].class,
        DeterministicContinuationClass::SourceCoverage
    );
    assert_eq!(
        partial_receipts[0].host_action,
        DeterministicContinuationHostAction::ReadMissingRanges
    );

    let full_payload = ToolPayload::Function {
        arguments: json!({
            "path": "src/lib.rs",
            "start_line": 1,
            "line_count": 5
        })
        .to_string(),
    };
    let (full, full_reads) =
        test_observation::observe(ReadFileSpanHandler::new(false).handle(ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker,
            call_id: "source-coverage-full".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: full_payload.clone(),
        }))
        .await;
    let full = full.expect("full overlap read");
    assert_eq!(
        full_reads.successful_content_reads, 0,
        "full overlap must not reread the source file",
    );
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        full.to_response_item("source-coverage-full", &full_payload)
    else {
        panic!("expected full function output");
    };
    let full_text = output.body.to_text().expect("full text");
    assert!(
        full_text.contains("already present in current context"),
        "{full_text}"
    );
    let full_receipts = full.deterministic_continuation_receipts();
    assert_eq!(full_receipts.len(), 1);
    assert_eq!(
        full_receipts[0].host_action,
        DeterministicContinuationHostAction::ReuseCoveredSpan
    );
}

#[tokio::test]
async fn completion_hook_invalidation_forces_immediate_source_reread_without_watcher_event() {
    let (mut session, mut turn) = make_session_and_context().await;
    session.services.git_workspace = GitWorkspaceCache::with_noop_watcher_for_tests();
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let source_cwd = source_dir.abs();
    std::fs::create_dir(source_cwd.join(".git").as_path()).expect("create git marker");
    std::fs::create_dir(source_cwd.join("src").as_path()).expect("create src");
    let source_path = source_cwd.join("src/lib.rs");
    std::fs::write(source_path.as_path(), "old\n").expect("write initial source");
    replace_primary_environment_cwd(&mut turn, source_cwd.clone());
    turn.permission_profile = PermissionProfile::Disabled;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
    let payload = ToolPayload::Function {
        arguments: json!({
            "path": "src/lib.rs",
            "start_line": 1,
            "line_count": 1
        })
        .to_string(),
    };

    let (first, first_reads) =
        test_observation::observe(ReadFileSpanHandler::new(false).handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn: Arc::clone(&turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::clone(&tracker),
            call_id: "completion-hook-source-before".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        }))
        .await;
    first.expect("initial source read");
    assert_eq!(first_reads.successful_content_reads, 1);

    let modified = std::fs::metadata(source_path.as_path())
        .and_then(|metadata| metadata.modified())
        .expect("initial source modified time");
    std::fs::write(source_path.as_path(), "new\n").expect("hook source mutation");
    std::fs::File::options()
        .write(true)
        .open(source_path.as_path())
        .and_then(|file| file.set_times(FileTimes::new().set_modified(modified)))
        .expect("restore source modified time");
    let result = WorkspaceMutationResult {
        lease_id: "completion-hook-source-mutation".to_string(),
        start_epoch: 0,
        end_epoch: 1,
        changed_paths: vec!["src/lib.rs".to_string()],
        drift_paths: Vec::new(),
    };
    assert!(record_finalized_workspace_mutation(
        session.services.git_workspace.as_ref(),
        source_cwd.as_path(),
        &result,
    ));

    let (second, second_reads) =
        test_observation::observe(ReadFileSpanHandler::new(false).handle(ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker,
            call_id: "completion-hook-source-after".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        }))
        .await;
    let second = second.expect("source read after completion hook mutation");
    assert_eq!(
        second_reads.successful_content_reads, 1,
        "the pre-hook replay artifact must be rejected synchronously"
    );
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        second.to_response_item("completion-hook-source-after", &payload)
    else {
        panic!("expected source function output");
    };
    let rendered = output.body.to_text().expect("source output text");
    assert!(rendered.contains("     1 | new"), "{rendered}");
}

#[tokio::test]
async fn concurrent_identical_reads_replay_after_reservation_wait() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let source_cwd = source_dir.abs();
    std::fs::create_dir(source_cwd.join("src").as_path()).expect("create src");
    let contents = std::iter::repeat_n("line of source evidence\n", 60_000).collect::<String>();
    std::fs::write(source_cwd.join("src/lib.rs").as_path(), contents).expect("write source");
    replace_primary_environment_cwd(&mut turn, source_cwd);
    turn.permission_profile = PermissionProfile::Disabled;
    let turn = Arc::new(turn);
    let session = Arc::new(session);
    let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
    let payload = ToolPayload::Function {
        arguments: json!({
            "path": "src/lib.rs",
            "start_line": 1,
            "line_count": 5
        })
        .to_string(),
    };
    let first_handler = ReadFileSpanHandler::new(false);
    let second_handler = ReadFileSpanHandler::new(false);
    let first_invocation = ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn: Arc::clone(&turn),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::clone(&tracker),
        call_id: "source-concurrent-first".to_string(),
        tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
        source: ToolCallSource::Direct,
        payload: payload.clone(),
    };
    let second_invocation = ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker,
        call_id: "source-concurrent-second".to_string(),
        tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
        source: ToolCallSource::Direct,
        payload,
    };

    let ((first, second), observation) = test_observation::observe(async {
        tokio::join!(
            first_handler.handle(first_invocation),
            second_handler.handle(second_invocation)
        )
    })
    .await;

    first.expect("first concurrent read");
    second.expect("second concurrent read");
    assert_eq!(observation.runtime_entries, 2);
    assert!(
        observation.read_reservation_waits >= 1,
        "the regression must exercise the post-reservation replay path"
    );
    assert_eq!(
        observation.successful_content_reads, 1,
        "the waiter should replay the authoritative coverage produced by the owner"
    );
}

#[tokio::test]
async fn bounded_read_bypasses_known_delta_and_preserves_fresh_worktree_evidence() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let source_cwd = source_dir.abs();
    let run_git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(source_cwd.as_path())
            .env("GIT_AUTHOR_NAME", "Codex Test")
            .env("GIT_AUTHOR_EMAIL", "codex@example.com")
            .env("GIT_COMMITTER_NAME", "Codex Test")
            .env("GIT_COMMITTER_EMAIL", "codex@example.com")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run_git(&["init"]);
    std::fs::create_dir(source_cwd.join("src").as_path()).expect("create src");
    let tracked = "tracked one\nmiddle\nend\n";
    let modified = "modified worktree contents\nmiddle\nend\n";
    let source_path = source_cwd.join("src/lib.rs");
    std::fs::write(source_path.as_path(), tracked).expect("write tracked source");
    run_git(&["add", "src/lib.rs"]);
    run_git(&["commit", "-m", "initial"]);

    replace_primary_environment_cwd(&mut turn, source_cwd);
    turn.permission_profile = PermissionProfile::Disabled;
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::KnownDeltaStore);
    turn.config = Arc::new(config);
    let turn = Arc::new(turn);
    let session = Arc::new(session);

    let default_payload = ToolPayload::Function {
        arguments: json!({
            "path": "src/lib.rs",
            "start_line": 1,
            "line_count": 3
        })
        .to_string(),
    };
    let ((default_result, default_reads), default_known_delta) =
        known_delta_store::test_observation::observe(test_observation::observe(
            ReadFileSpanHandler::new(false).handle(ToolInvocation {
                session: Arc::clone(&session),
                step_context: StepContext::for_test(Arc::clone(&turn)),
                turn: Arc::clone(&turn),
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "call-read-file-span-direct-default".to_string(),
                tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
                source: ToolCallSource::Direct,
                payload: default_payload.clone(),
            }),
        ))
        .await;
    let default_output = default_result.expect("default bounded read should succeed");
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        default_output.to_response_item("call-read-file-span-direct-default", &default_payload)
    else {
        panic!("expected function call output");
    };
    assert_eq!(output.success, Some(true));
    let text = output.body.to_text().expect("text output");
    assert!(text.contains("citation: src/lib.rs:1-3"), "{text}");
    assert!(text.contains("     1 | tracked one"), "{text}");
    assert_eq!(default_reads.successful_content_reads, 1);
    assert_eq!(default_known_delta.lookup_calls, 0);
    assert_eq!(default_known_delta.fingerprint_git_subprocesses, 0);

    std::fs::write(source_path.as_path(), modified).expect("modify tracked source");
    let fresh_payload = ToolPayload::Function {
        arguments: json!({
            "path": "src/lib.rs",
            "start_line": 1,
            "line_count": 3,
            "force_fresh": true
        })
        .to_string(),
    };
    let ((fresh_result, fresh_reads), fresh_known_delta) =
        known_delta_store::test_observation::observe(test_observation::observe(
            ReadFileSpanHandler::new(false).handle(ToolInvocation {
                session: Arc::clone(&session),
                step_context: StepContext::for_test(Arc::clone(&turn)),
                turn: Arc::clone(&turn),
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "call-read-file-span-direct-force-fresh".to_string(),
                tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
                source: ToolCallSource::Direct,
                payload: fresh_payload.clone(),
            }),
        ))
        .await;
    let fresh_output = fresh_result.expect("force-fresh bounded read should succeed");
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        fresh_output.to_response_item("call-read-file-span-direct-force-fresh", &fresh_payload)
    else {
        panic!("expected function call output");
    };
    assert_eq!(output.success, Some(true));
    let text = output.body.to_text().expect("text output");
    assert!(text.contains("citation: src/lib.rs:1-3"), "{text}");
    assert!(
        text.contains("     1 | modified worktree contents"),
        "{text}"
    );
    assert_eq!(fresh_reads.successful_content_reads, 1);
    assert_eq!(fresh_known_delta.lookup_calls, 0);
    assert_eq!(fresh_known_delta.fingerprint_git_subprocesses, 0);

    let coordinator = session.services.agent_control.task_coordinator();
    let store = coordinator
        .store()
        .expect("bounded read lazily initializes durable coordination");
    let actor_id = format!("root:{}", session.services.agent_control.session_id());
    let manifest = store
        .supporting_read_manifest(source_dir.path(), actor_id, vec!["src/lib.rs".to_string()])
        .await
        .expect("bounded read persists its supporting manifest");
    assert_eq!(manifest.len(), 1);
    assert_eq!(manifest[0].path, "src/lib.rs");
    assert!(manifest[0].existed);
    assert_eq!(
        manifest[0].content_hash.as_deref(),
        Some(format!("{:x}", Sha256::digest(modified.as_bytes())).as_str())
    );
}

#[tokio::test]
async fn supporting_read_coordination_cannot_hold_up_source_output() {
    let started = tokio::time::Instant::now();

    await_supporting_read_coordination(
        Duration::from_millis(10),
        std::future::pending::<Result<(), FunctionCallError>>(),
    )
    .await
    .expect("a stalled coordination write should not block confined source output");

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "coordination wait exceeded its bounded allowance"
    );
}

#[tokio::test]
async fn read_file_span_handler_reads_exact_loaded_skill_path() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    replace_primary_environment_cwd(&mut turn, source_dir.abs());
    turn.permission_profile = PermissionProfile::Disabled;

    let skill_dir = tempfile::tempdir().expect("create skill temp dir");
    let skill_path = skill_dir.abs().join("SKILL.md");
    std::fs::write(skill_path.as_path(), "first\ninstalled skill\nthird\n")
        .expect("write installed skill");
    let mut outcome = SkillLoadOutcome::default();
    outcome.skills = vec![SkillMetadata {
        name: "installed-test".to_string(),
        description: "test installed skill".to_string(),
        short_description: None,
        interface: None,
        dependencies: None,
        policy: None,
        path_to_skills_md: skill_path.clone(),
        scope: SkillScope::User,
        plugin_id: None,
    }];
    turn.turn_skills = crate::session::turn_context::TurnSkillsContext::new(
        HostSkillsSnapshot::new(Arc::new(outcome)),
    );
    let turn = Arc::new(turn);
    let payload = ToolPayload::Function {
        arguments: json!({
            "path": skill_path.to_string_lossy(),
            "start_line": 2,
            "line_count": 1
        })
        .to_string(),
    };

    let output = ReadFileSpanHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-read-loaded-skill".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .expect("loaded skill read should succeed");

    let ResponseInputItem::FunctionCallOutput { output, .. } =
        output.to_response_item("call-read-loaded-skill", &payload)
    else {
        panic!("expected function call output");
    };
    let text = output.body.to_text().expect("text output");
    assert!(text.contains("installed skill"), "{text}");
    assert!(text.contains("     2 | installed skill"), "{text}");
}

#[tokio::test]
async fn read_file_span_handler_resolves_opaque_skill_locator_and_cites_canonical_source() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    replace_primary_environment_cwd(&mut turn, source_dir.abs());
    turn.permission_profile = PermissionProfile::Disabled;

    let skill_dir = tempfile::tempdir().expect("create skill temp dir");
    let skill_path = skill_dir.abs().join("SKILL.md");
    std::fs::write(
        skill_path.as_path(),
        "first\nfull selected skill contents\nthird\n",
    )
    .expect("write installed skill");
    let skill = SkillMetadata {
        name: "opaque-installed-test".to_string(),
        description: "test opaque installed skill".to_string(),
        short_description: None,
        interface: None,
        dependencies: None,
        policy: None,
        path_to_skills_md: skill_path.clone(),
        scope: SkillScope::User,
        plugin_id: Some("test-plugin".to_string()),
    };
    let locator = format!("skill:{}", skill_catalog_id(&skill));
    let mut outcome = SkillLoadOutcome::default();
    outcome.skills = vec![skill];
    turn.turn_skills = crate::session::turn_context::TurnSkillsContext::new(
        HostSkillsSnapshot::new(Arc::new(outcome)),
    );
    let turn = Arc::new(turn);
    let payload = ToolPayload::Function {
        arguments: json!({
            "path": locator,
            "start_line": 1,
            "line_count": 3
        })
        .to_string(),
    };

    let output = ReadFileSpanHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-read-opaque-skill".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .expect("opaque loaded skill read should succeed");

    let ResponseInputItem::FunctionCallOutput { output, .. } =
        output.to_response_item("call-read-opaque-skill", &payload)
    else {
        panic!("expected function call output");
    };
    let text = output.body.to_text().expect("text output");
    assert!(text.contains("full selected skill contents"), "{text}");
    assert!(
        text.contains(&skill_path.to_string_lossy().replace('\\', "/")),
        "canonical source citation missing from {text}"
    );
}

#[tokio::test]
async fn read_file_span_handler_rejects_unloaded_path_outside_repository() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    replace_primary_environment_cwd(&mut turn, source_dir.abs());
    turn.permission_profile = PermissionProfile::Disabled;

    let outside_dir = tempfile::tempdir().expect("create outside temp dir");
    let outside_path = outside_dir.abs().join("outside.rs");
    std::fs::write(outside_path.as_path(), "outside\n").expect("write outside file");
    let turn = Arc::new(turn);
    let result = ReadFileSpanHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-read-unloaded-outside".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({ "path": outside_path.to_string_lossy() }).to_string(),
            },
        })
        .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected outside-repository rejection");
    };
    assert!(
        message.contains("resolves outside repository root"),
        "{message}"
    );
}

#[tokio::test]
async fn search_handler_honors_repository_gitignore_rules() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let source_cwd = source_dir.abs();
    std::fs::create_dir(source_cwd.join(".git").as_path()).expect("create git marker");
    std::fs::write(source_cwd.join(".gitignore").as_path(), "ignored/\n").expect("write gitignore");
    std::fs::create_dir(source_cwd.join("src").as_path()).expect("create source directory");
    std::fs::write(
        source_cwd.join("src/visible.rs").as_path(),
        "needle visible\n",
    )
    .expect("write visible source");
    std::fs::create_dir(source_cwd.join("ignored").as_path()).expect("create ignored directory");
    std::fs::write(
        source_cwd.join("ignored/hidden.rs").as_path(),
        "needle hidden\n",
    )
    .expect("write ignored source");
    replace_primary_environment_cwd(&mut turn, source_cwd);
    turn.permission_profile = PermissionProfile::Disabled;
    let turn = Arc::new(turn);
    let payload = ToolPayload::Function {
        arguments: json!({ "query": "needle", "paths": ["src", "ignored"] }).to_string(),
    };

    let output = SearchSourceHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-search-source-ignore".to_string(),
            tool_name: ToolName::plain(SEARCH_SOURCE_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .expect("source search should succeed");

    let ResponseInputItem::FunctionCallOutput { output, .. } =
        output.to_response_item("call-search-source-ignore", &payload)
    else {
        panic!("expected function call output");
    };
    let text = output.body.to_text().expect("text output");
    assert!(text.contains("coverage: files=1 "), "{text}");
    assert!(text.contains("citation: src/visible.rs:1-1 (match line 1)"));
    assert!(!text.contains("ignored/hidden.rs"), "{text}");
}

#[tokio::test]
async fn source_handlers_validate_bounds_before_environment_resolution() {
    let (search_session, search_turn) = make_session_and_context().await;
    let search_turn = Arc::new(search_turn);
    let (search_result, search_observation) = test_observation::observe(
        SearchSourceHandler::new(true).handle(ToolInvocation {
            session: Arc::new(search_session),
            step_context: StepContext::for_test(Arc::clone(&search_turn)),
            turn: search_turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-search-source-invalid-bounds".to_string(),
            tool_name: ToolName::plain(SEARCH_SOURCE_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({
                    "query": "needle",
                    "max_results": 0,
                    "environment_id": "missing-environment"
                })
                .to_string(),
            },
        }),
    )
    .await;
    let Err(FunctionCallError::RespondToModel(search_message)) = search_result else {
        panic!("expected search bound validation error");
    };
    assert!(search_message.contains("does not match its emitted schema"));
    assert!(search_message.contains("minimum"), "{search_message}");
    assert!(!search_message.contains("environment"), "{search_message}");
    assert_eq!(search_observation.runtime_entries, 0);

    let (read_session, read_turn) = make_session_and_context().await;
    let read_turn = Arc::new(read_turn);
    let (read_result, read_observation) = test_observation::observe(
        ReadFileSpanHandler::new(true).handle(ToolInvocation {
            session: Arc::new(read_session),
            step_context: StepContext::for_test(Arc::clone(&read_turn)),
            turn: read_turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-read-file-span-invalid-bounds".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({
                    "path": "missing.rs",
                    "line_count": 401,
                    "environment_id": "missing-environment"
                })
                .to_string(),
            },
        }),
    )
    .await;
    let Err(FunctionCallError::RespondToModel(read_message)) = read_result else {
        panic!("expected read bound validation error");
    };
    assert!(read_message.contains("does not match its emitted schema"));
    assert!(read_message.contains("maximum"), "{read_message}");
    assert!(!read_message.contains("environment"), "{read_message}");
    assert_eq!(read_observation.runtime_entries, 0);
}

#[test]
fn source_preflight_uses_the_emitted_schema_for_every_numeric_boundary() {
    fn assert_cases(
        tool_name: &str,
        contract: &SourceToolContract,
        cases: impl IntoIterator<Item = (serde_json::Value, bool)>,
    ) {
        let ToolSpec::Function(tool) = &contract.spec else {
            panic!("source tool must be a function");
        };
        let emitted_schema = serde_json::to_value(&tool.parameters).expect("serialize schema");
        let emitted_validator =
            jsonschema::validator_for(&emitted_schema).expect("compile emitted schema");
        for (arguments, expected_valid) in cases {
            let payload = ToolPayload::Function {
                arguments: arguments.to_string(),
            };
            assert_eq!(
                emitted_validator.is_valid(&arguments),
                expected_valid,
                "{tool_name}: emitted schema disagreed for {arguments}"
            );
            assert_eq!(
                contract.validate(tool_name, &payload).is_ok(),
                expected_valid,
                "{tool_name}: stored preflight disagreed for {arguments}"
            );
        }
    }

    let search = SearchSourceHandler::new(false);
    assert_cases(
        SEARCH_SOURCE_TOOL_NAME,
        &search.contract,
        [
            (json!({"query": "x", "max_results": 0}), false),
            (json!({"query": "x", "max_results": 1}), true),
            (
                json!({"query": "x", "max_results": SOURCE_SEARCH_MAX_MATCHES}),
                true,
            ),
            (
                json!({"query": "x", "max_results": SOURCE_SEARCH_MAX_MATCHES + 1}),
                false,
            ),
            (json!({"query": "x", "context_lines": 0}), true),
            (
                json!({"query": "x", "context_lines": SOURCE_SEARCH_MAX_CONTEXT_LINES}),
                true,
            ),
            (
                json!({"query": "x", "context_lines": SOURCE_SEARCH_MAX_CONTEXT_LINES + 1}),
                false,
            ),
        ],
    );

    let locate = LocateTaskHandler::new(false);
    assert_cases(
        LOCATE_TASK_TOOL_NAME,
        &locate.contract,
        [
            (json!({"task": "x", "max_files": 0}), false),
            (json!({"task": "x", "max_files": 1}), true),
            (
                json!({"task": "x", "max_files": LOCATE_TASK_MAX_FILES}),
                true,
            ),
            (
                json!({"task": "x", "max_files": LOCATE_TASK_MAX_FILES + 1}),
                false,
            ),
            (json!({"task": "x", "max_source_bytes": 0}), false),
            (json!({"task": "x", "max_source_bytes": 1}), true),
            (
                json!({"task": "x", "max_source_bytes": LOCATE_TASK_MAX_SOURCE_BYTES}),
                true,
            ),
            (
                json!({"task": "x", "max_source_bytes": LOCATE_TASK_MAX_SOURCE_BYTES + 1}),
                false,
            ),
        ],
    );

    let read = ReadFileSpanHandler::new(false);
    assert_cases(
        READ_FILE_SPAN_TOOL_NAME,
        &read.contract,
        [
            (json!({"path": "src/lib.rs", "start_line": 0}), false),
            (json!({"path": "src/lib.rs", "start_line": 1}), true),
            (json!({"path": "src/lib.rs", "line_count": 0}), false),
            (json!({"path": "src/lib.rs", "line_count": 1}), true),
            (
                json!({"path": "src/lib.rs", "line_count": SOURCE_READ_MAX_LINES}),
                true,
            ),
            (
                json!({"path": "src/lib.rs", "line_count": SOURCE_READ_MAX_LINES + 1}),
                false,
            ),
        ],
    );
}

#[tokio::test]
async fn source_handlers_reject_unknown_argument_names() {
    let (search_session, search_turn) = make_session_and_context().await;
    let search_turn = Arc::new(search_turn);
    let search_result = SearchSourceHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(search_session),
            step_context: StepContext::for_test(Arc::clone(&search_turn)),
            turn: search_turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-search-source-unknown-field".to_string(),
            tool_name: ToolName::plain(SEARCH_SOURCE_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({ "query": "needle", "context_line": 1 }).to_string(),
            },
        })
        .await;
    let Err(FunctionCallError::RespondToModel(search_message)) = search_result else {
        panic!("expected search parse error");
    };
    assert!(search_message.contains("does not match its emitted schema"));
    assert!(search_message.contains("context_line"));

    let (read_session, read_turn) = make_session_and_context().await;
    let read_turn = Arc::new(read_turn);
    let read_result = ReadFileSpanHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(read_session),
            step_context: StepContext::for_test(Arc::clone(&read_turn)),
            turn: read_turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-read-file-span-unknown-field".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({ "path": "src/lib.rs", "environment_ide": "local" }).to_string(),
            },
        })
        .await;
    let Err(FunctionCallError::RespondToModel(read_message)) = read_result else {
        panic!("expected read parse error");
    };
    assert!(read_message.contains("does not match its emitted schema"));
    assert!(read_message.contains("environment_ide"));
}

#[tokio::test]
async fn source_handlers_reject_environment_id_when_not_advertised() {
    let (search_session, search_turn) = make_session_and_context().await;
    let search_turn = Arc::new(search_turn);
    let search_result = SearchSourceHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(search_session),
            step_context: StepContext::for_test(Arc::clone(&search_turn)),
            turn: search_turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-search-source-unadvertised-environment".to_string(),
            tool_name: ToolName::plain(SEARCH_SOURCE_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({
                    "query": "needle",
                    "environment_id": "missing-environment"
                })
                .to_string(),
            },
        })
        .await;
    let Err(FunctionCallError::RespondToModel(search_message)) = search_result else {
        panic!("expected search parse error");
    };
    assert!(search_message.contains("does not match its emitted schema"));
    assert!(search_message.contains("environment_id"));

    let (read_session, read_turn) = make_session_and_context().await;
    let read_turn = Arc::new(read_turn);
    let read_result = ReadFileSpanHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(read_session),
            step_context: StepContext::for_test(Arc::clone(&read_turn)),
            turn: read_turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-read-file-span-unadvertised-environment".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({
                    "path": "src/lib.rs",
                    "environment_id": "missing-environment"
                })
                .to_string(),
            },
        })
        .await;
    let Err(FunctionCallError::RespondToModel(read_message)) = read_result else {
        panic!("expected read parse error");
    };
    assert!(read_message.contains("does not match its emitted schema"));
    assert!(read_message.contains("environment_id"));
}

#[tokio::test]
async fn source_handlers_select_local_environment_and_reject_remote_environment() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let source_cwd = source_dir.abs();
    std::fs::create_dir(source_cwd.join(".git").as_path()).expect("create git marker");
    std::fs::create_dir(source_cwd.join("src").as_path()).expect("create source directory");
    std::fs::write(source_cwd.join("src/lib.rs").as_path(), "needle\n").expect("write source file");
    replace_primary_environment_cwd(&mut turn, source_cwd);
    turn.permission_profile = PermissionProfile::Disabled;

    let local_environment_id = turn.environments.turn_environments[0]
        .environment_id
        .clone();
    let mut remote_environment = turn.environments.turn_environments[0].clone();
    remote_environment.environment_id = "remote-source".to_string();
    remote_environment.environment = Arc::new(
        Environment::create_for_tests(Some("ws://127.0.0.1:1/source-tools-remote".to_string()))
            .expect("remote test environment"),
    );
    let remote_environment_id = remote_environment.environment_id.clone();
    turn.environments.turn_environments.push(remote_environment);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let local_payload = ToolPayload::Function {
        arguments: json!({
            "query": "needle",
            "environment_id": local_environment_id,
        })
        .to_string(),
    };
    SearchSourceHandler::new(true)
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn: Arc::clone(&turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-search-source-local-selection".to_string(),
            tool_name: ToolName::plain(SEARCH_SOURCE_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: local_payload,
        })
        .await
        .expect("selected local source search should succeed");

    let remote_payload = ToolPayload::Function {
        arguments: json!({
            "query": "needle",
            "environment_id": "remote-source",
        })
        .to_string(),
    };
    let remote_result = SearchSourceHandler::new(true)
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn: Arc::clone(&turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-search-source-remote-selection".to_string(),
            tool_name: ToolName::plain(SEARCH_SOURCE_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: remote_payload,
        })
        .await;
    let Err(FunctionCallError::RespondToModel(message)) = remote_result else {
        panic!("expected remote source search to be rejected");
    };
    assert!(message.contains("source tools currently support local environments only"));

    ReadFileSpanHandler::new(true)
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn: Arc::clone(&turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-read-file-span-local-selection".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({
                    "path": "src/lib.rs",
                    "environment_id": local_environment_id,
                })
                .to_string(),
            },
        })
        .await
        .expect("local source read succeeds");

    let remote_read_result = ReadFileSpanHandler::new(true)
        .handle(ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-read-file-span-remote-selection".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({
                    "path": "src/lib.rs",
                    "environment_id": remote_environment_id,
                })
                .to_string(),
            },
        })
        .await;
    let Err(FunctionCallError::RespondToModel(message)) = remote_read_result else {
        panic!("expected remote source read to be rejected");
    };
    assert!(message.contains("source tools currently support local environments only"));
}

#[cfg(unix)]
fn create_directory_link(target: &std::path::Path, link: &std::path::Path) {
    std::os::unix::fs::symlink(target, link).expect("directory symlink is created");
}

#[cfg(windows)]
fn create_directory_link(target: &std::path::Path, link: &std::path::Path) {
    let status = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .status()
        .expect("junction command starts");
    assert!(status.success(), "directory junction is created");
}

#[cfg(unix)]
fn remove_directory_link(link: &std::path::Path) {
    std::fs::remove_file(link).expect("directory symlink is removed");
}

#[cfg(windows)]
fn remove_directory_link(link: &std::path::Path) {
    std::fs::remove_dir(link).expect("directory junction is removed");
}

#[tokio::test]
async fn read_file_span_handler_rejects_symlink_or_junction_escape() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let source_cwd = source_dir.abs();
    std::fs::create_dir(source_cwd.join(".git").as_path()).expect("create git marker");
    let outside_dir = tempfile::tempdir().expect("create outside temp dir");
    std::fs::write(outside_dir.path().join("secret.rs"), "outside secret\n")
        .expect("write outside source");
    let link_path = source_cwd.join("escape");

    create_directory_link(outside_dir.path(), link_path.as_path());

    replace_primary_environment_cwd(&mut turn, source_cwd);
    turn.permission_profile = PermissionProfile::Disabled;
    let turn = Arc::new(turn);
    let payload = ToolPayload::Function {
        arguments: json!({"path": "escape/secret.rs"}).to_string(),
    };
    let result = ReadFileSpanHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-read-file-span-symlink-escape".to_string(),
            tool_name: ToolName::plain(READ_FILE_SPAN_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload,
        })
        .await;
    remove_directory_link(link_path.as_path());
    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected symlink or junction escape to be rejected");
    };
    assert!(
        message.contains("resolves outside repository root"),
        "{message}"
    );
}

#[tokio::test]
async fn global_git_excludes_are_omitted_with_an_explicit_diagnostic() {
    let (session, mut turn) = make_session_and_context().await;
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let source_cwd = source_dir.abs();
    std::fs::create_dir(source_cwd.join(".git").as_path()).expect("create git marker");
    std::fs::write(source_cwd.join("ignored.rs").as_path(), "needle ignored\n")
        .expect("write ignored source");
    std::fs::write(source_cwd.join("visible.rs").as_path(), "needle visible\n")
        .expect("write visible source");
    replace_primary_environment_cwd(&mut turn, source_cwd);
    turn.permission_profile = PermissionProfile::Disabled;
    let turn = Arc::new(turn);
    let payload = ToolPayload::Function {
        arguments: json!({"query": "needle"}).to_string(),
    };
    let output = SearchSourceHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-load-global-git-excludes".to_string(),
            tool_name: ToolName::plain(SEARCH_SOURCE_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .expect("source search should succeed");

    let ResponseInputItem::FunctionCallOutput { output, .. } =
        output.to_response_item("call-load-global-git-excludes", &payload)
    else {
        panic!("expected function call output");
    };
    let text = output.body.to_text().expect("text output");
    assert!(text.contains("citation: ignored.rs:1-1"), "{text}");
    assert!(text.contains("citation: visible.rs:1-1"), "{text}");
    assert!(
        text.contains(
            "diagnostic: global Git ignore rules were omitted because the selected environment does not expose Git config resolution"
        ),
        "{text}"
    );
}

#[test]
fn owner_and_closure_states_gate_only_ready_bundles_for_implementation() {
    assert_eq!(
        directive_for_bundle_states(
            OwnerEvidenceOwnerState::OwnerUnresolved,
            OwnerEvidenceClosureState::BundleIncomplete,
        ),
        "focused_evidence_followup"
    );
    assert_eq!(
        directive_for_bundle_states(
            OwnerEvidenceOwnerState::OwnerResolved,
            OwnerEvidenceClosureState::BundleIncomplete,
        ),
        "focused_evidence_followup"
    );
    assert_eq!(
        directive_for_bundle_states(
            OwnerEvidenceOwnerState::OwnerUnresolved,
            OwnerEvidenceClosureState::BundleReady,
        ),
        "owner_resolution_followup"
    );
    assert_eq!(
        directive_for_bundle_states(
            OwnerEvidenceOwnerState::OwnerResolved,
            OwnerEvidenceClosureState::BundleReady,
        ),
        "implementation_phase"
    );
}

#[test]
fn instruction_freshness_joins_validated_core_and_locator_snapshot_identities() {
    let captured = codex_file_search::task_locator::LocateTaskSourceIdentity {
        path: "AGENTS.md".to_string(),
        content_hash: "hash-a".to_string(),
        source_snapshot_identity: "snapshot-a".to_string(),
    };
    assert!(validated_instruction_identity_is_current(
        Some(&captured),
        "snapshot-a",
        "hash-a",
        Some("hash-a"),
    ));
    assert!(!validated_instruction_identity_is_current(
        Some(&captured),
        "snapshot-b",
        "hash-a",
        Some("hash-a"),
    ));
    assert!(!validated_instruction_identity_is_current(
        Some(&captured),
        "snapshot-a",
        "hash-b",
        Some("hash-a"),
    ));
    assert!(!validated_instruction_identity_is_current(
        None,
        "snapshot-a",
        "hash-a",
        Some("hash-a"),
    ));
}

#[test]
fn bundle_receipt_identity_uses_only_stable_contract_inputs() {
    let instruction_snapshot = InstructionSnapshotIdentity::loaded([7; 32]);
    let first = stable_bundle_receipt_id(
        "epoch-7",
        Some("owner"),
        "snapshot",
        &instruction_snapshot,
        "closure-v2",
    );
    let second = stable_bundle_receipt_id(
        "epoch-7",
        Some("owner"),
        "snapshot",
        &instruction_snapshot,
        "closure-v2",
    );
    assert_eq!(first, second);
    assert_ne!(
        first,
        stable_bundle_receipt_id(
            "epoch-8",
            Some("owner"),
            "snapshot",
            &instruction_snapshot,
            "closure-v2",
        )
    );
    assert_ne!(
        first,
        stable_bundle_receipt_id(
            "epoch-7",
            Some("owner"),
            "snapshot-2",
            &instruction_snapshot,
            "closure-v2",
        )
    );
    assert_ne!(
        first,
        stable_bundle_receipt_id(
            "epoch-7",
            Some("owner"),
            "snapshot",
            &InstructionSnapshotIdentity::loaded([8; 32]),
            "closure-v2",
        )
    );
}

#[test]
fn canonical_bundle_artifact_contains_every_materialized_section_and_no_false_range() {
    let snapshot = "snapshot-a";
    let primary_text = "fn primary() {}\n";
    let primary_hash = sha256_text(primary_text);
    let primary_file_hash = "primary-file-hash".to_string();
    let missing_file_hash = "missing-file-hash".to_string();
    let output = LocateTaskOutput {
        request_identity: "locator-request".to_string(),
        rendered: String::new(),
        supporting_reads: vec![
            codex_file_search::task_locator::SupportingRead {
                path: "src/main.rs".to_string(),
                content_hash: primary_file_hash.clone(),
            },
            codex_file_search::task_locator::SupportingRead {
                path: "tests/main.rs".to_string(),
                content_hash: missing_file_hash.clone(),
            },
        ],
        snapshot_id: snapshot.to_string(),
        decision_facts: LocateTaskDecisionFacts {
            repository_identity: "repository".to_string(),
            source_snapshot_identity: snapshot.to_string(),
            owner_manifest_revision: "manifest".to_string(),
            closure_contract_revision: "source_closure_v2".to_string(),
            completeness: "complete".to_string(),
            selected_owner: Some("owner".to_string()),
            authoritative_owner: None,
            owner_candidates: Vec::new(),
            primary_path: Some("src/main.rs".to_string()),
            primary_symbol: Some("primary".to_string()),
            primary_span: None,
            source_relationships: Vec::new(),
            located_contracts: Vec::new(),
            located_tests: vec![codex_file_search::task_locator::LocateTaskLocatedPath {
                path: "tests/main.rs".to_string(),
                role: "test".to_string(),
            }],
            captured_instruction_sources: Vec::new(),
            captured_source_sections: vec![
                LocateTaskSourceSection {
                    section_id: "primary-section".to_string(),
                    kind: LocateTaskSourceSectionKind::PrimaryImplementation,
                    state: LocateTaskSourceSectionState::Materialized,
                    path: "src/main.rs".to_string(),
                    span: None,
                    content_hash: Some(primary_hash.clone()),
                    file_content_hash: Some(primary_file_hash),
                    source_snapshot_identity: snapshot.to_string(),
                    text: Some(primary_text.to_string()),
                    provenance: "test".to_string(),
                },
                LocateTaskSourceSection {
                    section_id: "missing-test-section".to_string(),
                    kind: LocateTaskSourceSectionKind::Test,
                    state: LocateTaskSourceSectionState::NotMaterialized,
                    path: "tests/main.rs".to_string(),
                    span: Some(codex_file_search::task_locator::ExactSpan {
                        start_line: 10,
                        end_line: 20,
                        start_byte: 90,
                        end_byte: 200,
                    }),
                    content_hash: None,
                    file_content_hash: Some(missing_file_hash),
                    source_snapshot_identity: snapshot.to_string(),
                    text: None,
                    provenance: "test".to_string(),
                },
            ],
            candidate_validation_routes: Vec::new(),
            source_gaps: Vec::new(),
            unresolved_source_ambiguity: Vec::new(),
            truncated: false,
        },
        owner_packet_seed: None,
        files_inspected: 2,
        files_reparsed: 2,
        rendered_bytes: 0,
    };

    let bundle = assemble_owner_evidence_bundle_v2(
        output,
        "epoch",
        None,
        None,
        InstructionSnapshotIdentity::Empty,
        &BTreeMap::new(),
    );
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        bundle.to_response_item("call", &payload)
    else {
        panic!("expected bundle function output");
    };
    let artifact = output.body.to_text().expect("bundle artifact text");
    let lines = artifact.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "OWNER_EVIDENCE_BUNDLE_V2");
    let metadata: serde_json::Value = serde_json::from_str(lines[1]).expect("bundle metadata");
    let manifest = metadata["section_manifest"]
        .as_array()
        .expect("section manifest");
    let primary = manifest
        .iter()
        .find(|entry| entry["section_id"] == "primary-section")
        .expect("primary manifest entry");
    let primary_line = primary["artifact_line_range"]["start_line"]
        .as_u64()
        .expect("primary artifact line") as usize;
    assert_eq!(primary["artifact_line_range"]["end_line"], primary_line);
    let primary_record: serde_json::Value =
        serde_json::from_str(lines[primary_line - 1]).expect("primary artifact record");
    assert_eq!(primary_record["exact_text"], primary_text);
    assert_eq!(primary_record["content_hash"], primary_hash);

    let code_mode = bundle.code_mode_result(&payload);
    assert_eq!(code_mode["bundle"], metadata);
    let materialized_sections = code_mode["materialized_sections"]
        .as_array()
        .expect("native materialized sections");
    let native_primary = materialized_sections
        .iter()
        .find(|section| section["section_id"] == "primary-section")
        .expect("native primary section");
    assert_eq!(native_primary["exact_text"], primary_text);
    assert_eq!(native_primary["content_hash"], primary_hash);

    let canonical = bundle
        .canonical_result(&payload)
        .expect("owner evidence canonical text");
    assert_eq!(canonical.kind, codex_tools::CanonicalToolResultKind::Text);
    assert_eq!(canonical.bytes, artifact.as_bytes());
    let projection = bundle
        .projection_metadata()
        .expect("owner evidence projection");
    assert_eq!(projection.predetermined_ranges.len(), 1);
    assert_eq!(projection.predetermined_ranges[0].id, "primary-section");
    assert_eq!(
        (
            projection.predetermined_ranges[0].start_line,
            projection.predetermined_ranges[0].end_line,
        ),
        (primary_line, primary_line)
    );
    let primary_fragment = projection
        .fragments
        .iter()
        .find(|fragment| fragment.id.as_deref() == Some("primary-section"))
        .expect("primary exact fragment");
    assert_eq!(primary_fragment.text, lines[primary_line - 1]);

    let missing = manifest
        .iter()
        .find(|entry| entry["section_id"] == "missing-test-section")
        .expect("missing manifest entry");
    assert_eq!(missing["state"], "not_materialized");
    assert!(missing["artifact_line_range"].is_null());
    let signal = bundle
        .sampling_request_signal()
        .expect("owner bundle signal");
    assert_eq!(
        signal["required_source_sections"],
        serde_json::json!([{
            "path": "tests/main.rs",
            "obligation_kind": "test",
            "span": {
                "start_line": 10,
                "end_line": 20,
                "start_byte": 90,
                "end_byte": 200,
            },
        }])
    );
}
