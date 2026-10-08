use super::*;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::SearchToolCallParams;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn epistemic_diagnostic_coordinates_preserve_owners_and_nonlocations() {
    for (first, second) in [
        ("at C:\\work\\test.rs:12:9\r\nFinished test in 1s", "at C:\\work\\test.rs:30:2\r\nFinished test in 2s"),
        ("  --> src/é.rs:12:3", "  --> src/é.rs:23:4"),
    ] {
        assert_eq!(normalize_tool_failure_text(first), normalize_tool_failure_text(second));
    }
    for text in ["localhost:8080", "expected 120ms", "assertion: file.rs:value", "no location\r\n", ""] {
        assert_eq!(normalize_tool_failure_text(text), text);
    }
    assert_ne!(normalize_tool_failure_text("at a.rs:12:9"), normalize_tool_failure_text("at b.rs:12:9"));
    assert_eq!(normalize_tool_failure_text("at a.rs:12:9\n"), "at a.rs:<location>\n");
}

#[test]
fn evidence_salience_stop_feedback_preserves_measurements_and_deadlines() {
    for (first, second) in [
        ("Latency gate failed: p95 is 120ms; limit is 100ms", "Latency gate failed: p95 is 950ms; limit is 100ms"),
        ("deadline is 12:00:00", "deadline is 13:00:00"),
        ("expected 1s", "expected 2s"),
        ("cutoff 2026-10-01T12:00:00Z", "cutoff 2026-10-02T12:00:00Z"),
    ] {
        assert_ne!(normalize_observation_text(first), normalize_observation_text(second));
    }
    assert_eq!(normalize_observation_text("2026-10-01T12:00:00Z ERROR gate rejected\nFinished test in 1s"),
        normalize_observation_text("2026-10-02T12:00:00Z ERROR gate rejected\nFinished test in 2s"));
}

#[test]
fn evidence_salience_successful_validation_preserves_owners_and_exact_values() {
    let validation = crate::validation::CommandValidation {
        execution_context: None, declared: None,
        classification: crate::validation::classify_validation_script("cargo check"),
        receipt_runner: None,
    };
    for (first, second) in [
        ("src/a.rs:10:warning: unused key", "src/b.rs:10:warning: unused key"),
        ("warning: unused key\n --> src/a.rs:2:1\n2 | let key = 1;", "warning: unused key\n --> src/b.rs:2:1\n2 | let key = 1;"),
        ("warning: threshold 120ms", "warning: threshold 950ms"),
        ("  actual: x", " actual: x"),
    ] {
        let evidence = |text: &str| {
            let direct = successful_command_evidence(text.as_bytes(), Some("cargo check"));
            let mut signal = json!({});
            attach_command_validation(&mut signal, text.as_bytes(), Some(&validation), Some(0), true);
            assert_eq!(signal["semantic_evidence"], json!(direct));
            direct
        };
        assert_ne!(evidence(first), evidence(second));
    }
    assert_eq!(successful_command_evidence(b"src/a.rs:10:2: warning: key\nFinished test in 1s", Some("cargo check")),
        successful_command_evidence(b"src/a.rs:20:3: warning: key\nFinished test in 2s", Some("cargo check")));
    assert_ne!(successful_command_evidence(b"Finished test in 1s", Some("cat report.txt")),
        successful_command_evidence(b"Finished test in 2s", Some("cat report.txt")));
    assert_ne!(successful_command_evidence(&[0xff, b'a'], Some("cargo check")),
        successful_command_evidence("�a".as_bytes(), Some("cargo check")));
}

#[test]
fn windows_crash_exit_codes_are_named_and_ordinary_codes_are_not() {
    let notice = windows_abnormal_exit_notice(-1_073_741_819).expect("access violation");
    assert!(
        notice.contains("0xC0000005 STATUS_ACCESS_VIOLATION"),
        "{notice}"
    );
    assert!(notice.contains("crashed"), "{notice}");
    assert!(
        windows_abnormal_exit_notice(-1_073_740_791)
            .is_some_and(|notice| notice.contains("STATUS_STACK_BUFFER_OVERRUN"))
    );
    for ordinary in [0, 1, 2, 101, 127, -1] {
        assert_eq!(windows_abnormal_exit_notice(ordinary), None);
    }
}

#[test]
fn command_evidence_ignores_durations_and_timestamps() {
    for (first, second) in [
        ("Finished test in 2.31s", "Finished test in 9.84s"),
        ("1 failed in 0.12s", "1 failed in 1.55s"),
        ("2026-09-29T12:03:01.555Z error: broken", "2026-09-30T01:02:03Z error: broken"),
        ("2026-09-29 12:03:01 error: broken", "2026-09-30 01:02:03 error: broken"),
    ] {
        assert_eq!(semantic_evidence_for_command_output(first.as_bytes()),
            semantic_evidence_for_command_output(second.as_bytes()));
    }
    assert_ne!(semantic_evidence_for_command_output(b"1 failed in 0.12s"),
        semantic_evidence_for_command_output(b"2 failed in 0.12s"));
    assert_ne!(semantic_evidence_for_command_output(b"localhost:8080"),
        semantic_evidence_for_command_output(b"localhost:9090"));
    // Generic prose has no producer framing: its coordinates and durations
    // can be substantive facts rather than volatile diagnostics.
    assert_ne!(normalize_tool_failure_text("invalid at line 12 column 9 in 1.5s"),
        normalize_tool_failure_text("invalid at line 30 column 2 in 8.0s"));
    assert_eq!(normalize_tool_failure_text("at example.rs:12:9\nFinished test in 1.5s"),
        normalize_tool_failure_text("at example.rs:30:2\nFinished test in 8.0s"));
}

#[test]
fn command_timing_normalization_preserves_substantive_data_and_failure_status() {
    for (first, second) in [
        (
            "test result: FAILED. 2 passed; 1 failed; 0 ignored; finished in 0.01s",
            "test result: FAILED. 2 passed; 1 failed; 0 ignored; finished in 4.25s",
        ),
        ("Finished `test` profile [unoptimized] in 0.12s", "Finished `test` profile [unoptimized] in 9.13s"),
        ("================ 2 passed, 1 skipped in 0.12s ================", "================ 2 passed, 1 skipped in 8.32s ================"),
        ("Summary [0.112s] 2 tests run: 1 passed, 1 failed", "Summary [2.541s] 2 tests run: 1 passed, 1 failed"),
    ] {
        let first_evidence = semantic_evidence_for_command_output(first.as_bytes());
        let second_evidence = semantic_evidence_for_command_output(second.as_bytes());
        assert_eq!(first_evidence, second_evidence, "{first}");
        assert_eq!(
            command_failure_signature(&first_evidence, Some(1)),
            command_failure_signature(&second_evidence, Some(1)),
        );
        assert_ne!(
            command_failure_signature(&first_evidence, Some(1)),
            command_failure_signature(&second_evidence, Some(2)),
        );
    }
    for (first, second) in [
        ("timeout = 1s", "timeout = 2s"),
        ("assertion failed: expected 1s", "assertion failed: expected 2s"),
        ("error: deadline was 1s", "error: deadline was 2s"),
        ("2026-09-29T12:00:00Z", "2026-09-30T12:00:00Z"),
        ("test result: ok. 1 passed; finished in 1s", "test result: ok. 2 passed; finished in 1s"),
        ("2026-09-29T12:00:00Z ERROR timeout = 1s", "2026-09-29T12:00:00Z ERROR timeout = 2s"),
        (
            "diff --git a/a b/a\n@@ -1 +1 @@\n+Finished test in 1s",
            "diff --git a/a b/a\n@@ -1 +1 @@\n+Finished test in 2s",
        ),
    ] {
        assert_ne!(
            semantic_evidence_for_command_output(first.as_bytes()),
            semantic_evidence_for_command_output(second.as_bytes()),
            "{first}",
        );
    }
}

#[test]
fn exec_validation_timing_is_stable_without_normalizing_source_reads() {
    let output = |command: &str, seconds: &str, exit_code: i32| ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "timing".into(),
        chunk_id: "chunk".into(),
        wall_time: std::time::Duration::from_secs(1),
        raw_output: if exit_code == 1 {
            format!("FAIL [{seconds}s] example::fails\nthread 'example::fails' panicked at example.rs:12:3:\nassertion failed\n").into_bytes()
        } else {
            format!("test result: ok. 1 passed; 0 failed; finished in {seconds}s").into_bytes()
        },
        truncation_policy: TruncationPolicy::Tokens(1000),
        max_output_tokens: None,
        process_id: None,
        session_capabilities: None,
        exit_code: Some(exit_code),
        process_exited: true,
        search_no_match: false,
        original_token_count: None,
        hook_command: Some(command.into()),
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };
    for (command, exit_code, same) in [
        ("cargo test -p example", 0, true),
        ("cargo test -p example", 1, true),
        ("cat results.txt", 0, false),
    ] {
        let first = output(command, "0.10", exit_code);
        let second = output(command, "0.90", exit_code);
        assert_ne!(first.raw_output, second.raw_output);
        let first = first.sampling_request_signal().unwrap();
        let second = second.sampling_request_signal().unwrap();
        assert_eq!(first["semantic_evidence"] == second["semantic_evidence"], same);
        if exit_code == 1 {
            assert_eq!(first["failure_signature"], second["failure_signature"]);
        }
    }
}

#[test]
fn orchestration_audit_tool_dispatch_state_has_one_terminal_transition_owner() {
    let completed = ToolDispatchState::new();
    assert!(completed.try_admit());
    assert!(completed.try_complete());
    assert_eq!(completed.try_abort(), ToolDispatchAbort::AlreadyTerminal);
    assert!(completed.is_terminal());
    assert!(!completed.is_aborted());

    let cancelled_before_admission = ToolDispatchState::new();
    assert_eq!(
        cancelled_before_admission.try_abort(),
        ToolDispatchAbort::BeforeAdmission
    );
    assert!(!cancelled_before_admission.try_admit());
    assert!(!cancelled_before_admission.try_complete());
    assert!(cancelled_before_admission.is_aborted());

    let cancelled_after_admission = ToolDispatchState::new();
    assert!(cancelled_after_admission.try_admit());
    assert_eq!(
        cancelled_after_admission.try_abort(),
        ToolDispatchAbort::AfterAdmission
    );
    assert!(!cancelled_after_admission.try_complete());
    assert!(cancelled_after_admission.is_aborted());
}

#[test]
fn parallel_test_failures_ignore_runner_noise_but_preserve_failure_changes() {
    let first = concat!(
        "run id: 1234\nFAIL [ 1.793s] (1/2) suite case_a\n",
        "---- STDERR: (1/2) suite case_a ----\n",
        "thread 'case_a' (101) panicked at src/a.rs:10:2:\n",
        "assertion failed: expected 1s; log_id=abcd\n",
        "FAIL [ 2.420s] (2/2) suite case_b\n",
        "---- STDERR: (2/2) suite case_b ----\n",
        "thread 'case_b' (102) panicked at src/b.rs:20:3:\n",
        "missing fixture\nSummary [4.213s] 2 tests run: 2 failed\n",
    );
    let reordered = concat!(
        "run id: 5678\nFAIL [ 9.003s] (1/2) suite case_b\n",
        "---- STDERR: (1/2) suite case_b ----\n",
        "thread 'case_b' (555) panicked at src/b.rs:20:3:\n",
        "missing fixture\nFAIL [ 4.002s] (2/2) suite case_a\n",
        "---- STDERR: (2/2) suite case_a ----\n",
        "thread 'case_a' (666) panicked at src/a.rs:10:2:\n",
        "assertion failed: expected 1s; log_id=abcd\n",
        "Summary [13.005s] 2 tests run: 2 failed\n",
    );
    let fingerprint = |text: &str| command_failure_signature(
        &failed_command_evidence(text.as_bytes(), Some("cargo nextest run")), Some(1),
    );
    assert_eq!(fingerprint(first), fingerprint(reordered));
    assert_ne!(fingerprint(first), fingerprint(&format!("{first}TIMEOUT [ 30.0s] (3/3) suite case_c\n")));
    assert_ne!(fingerprint(first), fingerprint(&first.replace("log_id=abcd", "log_id=efgh")));
    assert_ne!(fingerprint(first), fingerprint(&first.replace("missing fixture", " missing fixture")));
    let skipped = format!("{first}SKIP [ 0.001s] (3/3) suite sibling\n");
    assert_ne!(fingerprint(&skipped), fingerprint(&skipped.replace("SKIP ", "TIMEOUT ")));
    assert_ne!(fingerprint(first), fingerprint(&first.replace("2 tests run: 2 failed", "3 tests run: 2 failed, 1 timed out")));
    let unknown = format!("unknown diagnostic payload\n{first}");
    assert_eq!(failed_command_evidence(unknown.as_bytes(), Some("cargo nextest run")), canonical_output_evidence(unknown.as_bytes()));
    assert_ne!(fingerprint(first), fingerprint(&reordered.replace("case_b", "case_c")));
    assert_ne!(fingerprint(first), fingerprint(&reordered.replace("expected 1s", "expected 2s")));
    let assertion = first.replace("assertion failed: expected 1s; log_id=abcd",
        "assertion `left == right` failed\n  left: 10\n right: 20");
    assert_ne!(fingerprint(&assertion), fingerprint(&assertion.replace("left: 10", "left: 11")));
    let identifier = assertion.replace("left: 10", "left: run_id=10");
    assert_ne!(fingerprint(&identifier), fingerprint(&identifier.replace("left: run_id=10", "left: run_id=11")));
    assert_ne!(fingerprint(first), fingerprint(&first
        .replace("thread 'case_a'", "thread 'swapped'")
        .replace("thread 'case_b'", "thread 'case_a'")
        .replace("thread 'swapped'", "thread 'case_b'")));
    assert_ne!(successful_command_evidence(first.as_bytes(), Some("cat failure.txt")),
        successful_command_evidence(reordered.as_bytes(), Some("cat failure.txt")));
    assert_ne!(
        test_failure_evidence(b"FAILED tests/a.py::test_one - AssertionError: run_id=123 expected 1s\n"),
        test_failure_evidence(b"FAILED tests/a.py::test_one - AssertionError: run_id=456 expected 1s\n"),
    );
}

#[test]
fn search_evidence_preserves_exact_output_including_file_order() {
    let first = b"a.rs:1:first\na.rs:2:second\nb.rs:1:other\n";
    let reordered = b"b.rs:1:other\na.rs:1:first\na.rs:2:second\n";
    let search = |bytes: &[u8]| successful_command_evidence(bytes, Some("rg -n pattern src"));
    assert_ne!(search(first), search(reordered));
    assert_ne!(search(first), search(b"a.rs:2:second\na.rs:1:first\nb.rs:1:other\n"));
    assert_ne!(search(first), search(b"a.rs:1:changed\na.rs:2:second\nb.rs:1:other\n"));
    for command in ["cat report.txt", "rg -n pattern src; cat report.txt", "rg -n --json pattern src"] {
        assert_ne!(successful_command_evidence(first, Some(command)),
            successful_command_evidence(reordered, Some(command)));
    }
}

#[test]
fn verified_evidence_failed_command_preserves_unknown_owner_and_whitespace() {
    for command in [None, Some("custom-producer"), Some("cargo check")] {
        let first = failed_command_evidence(b"src/a.rs:7:3: failed\n  actual: x\n", command);
        assert_ne!(first, failed_command_evidence(b"src/b.rs:7:3: failed\n  actual: x\n", command));
        assert_ne!(first, failed_command_evidence(b"src/a.rs:7:3: failed\n actual: x\n", command));
    }
    assert_ne!(failed_command_evidence(b"a:7\n", None), failed_command_evidence(b"a:9\n", None));
    let validation = crate::validation::CommandValidation {
        execution_context: None,
        declared: None,
        classification: crate::validation::classify_validation_script("cargo check"),
        receipt_runner: None,
    };
    let evidence = |bytes: &[u8]| {
        let mut signal = serde_json::json!({});
        attach_command_validation(&mut signal, bytes, Some(&validation), Some(1), true);
        signal["semantic_evidence"].clone()
    };
    assert_ne!(evidence(b"src/a.rs:7:3: failed\n"), evidence(b"src/b.rs:7:3: failed\n"));
    assert_ne!(evidence(b"src/a.rs:7:3: failed\n  actual: x\n"), evidence(b"src/a.rs:7:3: failed\n actual: x\n"));
}

fn mcp_tool_output(
    result: CallToolResult,
    wall_time: std::time::Duration,
    original_image_detail_supported: bool,
    truncation_policy: TruncationPolicy,
) -> McpToolOutput {
    McpToolOutput::new(
        result,
        json!({}),
        wall_time,
        original_image_detail_supported,
        truncation_policy,
    )
}

#[test]
fn custom_tool_calls_should_roundtrip_as_custom_outputs() {
    let payload = ToolPayload::Custom {
        input: "patch".to_string(),
    };
    let response = FunctionToolOutput::from_text("patched".to_string(), Some(true))
        .to_response_item("call-42", &payload);

    match response {
        ResponseInputItem::CustomToolCallOutput {
            call_id, output, ..
        } => {
            assert_eq!(call_id, "call-42");
            assert_eq!(output.content_items(), None);
            assert_eq!(output.body.to_text().as_deref(), Some("patched"));
            assert_eq!(output.success, Some(true));
        }
        other => panic!("expected CustomToolCallOutput, got {other:?}"),
    }
}

#[test]
fn function_payloads_remain_function_outputs() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let response = FunctionToolOutput::from_text("ok".to_string(), Some(true))
        .to_response_item("fn-1", &payload);

    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "fn-1");
            assert_eq!(output.content_items(), None);
            assert_eq!(output.body.to_text().as_deref(), Some("ok"));
            assert_eq!(output.success, Some(true));
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn omitted_function_output_status_is_not_reported_as_success() {
    let output = FunctionToolOutput::from_text("status unavailable".to_string(), None);

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    assert!(!output.success_for_logging());
    assert_eq!(
        output
            .projection_metadata()
            .map(|metadata| metadata.outcome),
        Some(ToolOutputOutcome::Failure)
    );
}

#[test]
fn apply_patch_code_mode_result_preserves_output() {
    let text = "Success. Updated the following files:\nA code_mode_apply_patch.txt\n".to_string();
    let output = ApplyPatchToolOutput::from_text(text.clone());

    assert_eq!(
        output.code_mode_result(&ToolPayload::Function {
            arguments: "{}".to_string(),
        }),
        json!({
            "success": true,
            "text": text,
            "changes": [],
            "changes_exact": true,
            "environment_id": null,
            "diagnostics": [],
        })
    );
}

#[tokio::test]
async fn applied_patch_diffs_report_committed_lines_and_bounds() {
    let root = tempfile::tempdir().unwrap();
    let cwd = codex_utils_path_uri::PathUri::from_host_native_path(root.path()).unwrap();
    std::fs::write(root.path().join("old.txt"), "one\ntwo\n").unwrap();
    std::fs::write(root.path().join("deleted.txt"), "deleted\n").unwrap();
    std::fs::write(root.path().join("overwrite.txt"), "displaced\n").unwrap();
    let mut patch = "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n@@\n one\n-two\n+λ changed\n*** Delete File: deleted.txt\n*** Add File: overwrite.txt\n+replacement\n".to_string();
    for index in 0..5 {
        patch.push_str(&format!("*** Add File: large{index}.txt\n+{}\n", "λ".repeat(6000)));
    }
    patch.push_str("*** End Patch");
    let delta = codex_apply_patch::apply_patch(&patch, &cwd, &mut Vec::new(), &mut Vec::new(),
        codex_exec_server::LOCAL_FS.as_ref(), None).await.unwrap();
    // The receipt is built from committed bytes, not a post-patch read.
    std::fs::write(root.path().join("new.txt"), "independent later edit\n").unwrap();
    let output = ApplyPatchToolOutput::from_delta("applied".into(), true, &delta, None);
    let changed = output.changes.iter().find(|change| change["kind"] == "update").unwrap();
    assert_eq!(changed["unified_diff"], "@@ -1,2 +1,2 @@\n");
    assert_eq!(changed["diff_format"], "hunk_headers");
    assert_eq!(changed["diff_complete"], true);
    assert!(changed["move_path"].as_str().unwrap().ends_with("new.txt"));
    for change in output.changes.iter().filter(|change| change["path"].as_str().unwrap().contains("large")) {
        assert!(change.get("unified_diff").is_none());
    }
    let overwritten = output.changes.iter().find(|change| change["path"].as_str().unwrap().ends_with("overwrite.txt")).unwrap();
    assert!(overwritten["unified_diff"].as_str().unwrap().contains("-displaced"));
    assert!(!output.model_text().contains("+λ changed"));
    // Failed/partial operations keep the complete bounded diff representation.
    let output = ApplyPatchToolOutput::from_delta("partial failure".into(), false, &delta, None);
    let changed = output.changes.iter().find(|change| change["kind"] == "update").unwrap();
    assert!(changed["unified_diff"].as_str().unwrap().contains("@@ -1,2 +1,2 @@"));
    assert!(changed["unified_diff"].as_str().unwrap().contains("+λ changed"));
    assert_eq!(changed["diff_complete"], true);
    assert!(changed["move_path"].as_str().unwrap().ends_with("new.txt"));
    let deleted = output.changes.iter().find(|change| change["kind"] == "delete").unwrap();
    assert!(deleted["unified_diff"].as_str().unwrap().contains("-deleted"));
    assert!(output.changes.iter().any(|change| change["diff_complete"] == false));
    let mut total = 0;
    for change in &output.changes {
        let diff = change["unified_diff"].as_str().unwrap();
        assert!(diff.len() <= 8 * 1024);
        total += diff.len();
        assert_eq!(change["diff_complete"] == true, diff.len() as u64 == change["diff_bytes"].as_u64().unwrap());
    }
    assert!(total <= 32 * 1024);
    assert!(output.model_text().contains("+λ changed"));
}

#[test]
fn failed_patch_carries_its_retry_receipt_once_per_projection() {
    let receipt = json!({"patch_id": "patch-1", "remaining_hunks": [{"hunk": 2, "chunks": 3}]});
    let output = ApplyPatchToolOutput {
        text: "apply_patch verification failed: Ambiguous exact match".to_string(),
        success: false,
        changes: Vec::new(),
        changes_exact: true,
        environment_id: None,
        retry: None,
        diagnostics: Vec::new(),
    }
    .with_retry(Some(receipt.clone()));
    let payload = ToolPayload::Custom {
        input: "*** Begin Patch\n*** End Patch".to_string(),
    };

    let structured = output.code_mode_result(&payload);
    assert_eq!(structured["retry"], receipt);
    assert_eq!(
        structured.to_string().matches("patch-1").count(),
        1,
        "{structured}"
    );
    let ResponseInputItem::CustomToolCallOutput { output: direct, .. } =
        output.to_response_item("patch-call", &payload)
    else {
        panic!("expected custom tool output");
    };
    let direct = direct.body.to_text().unwrap_or_default();
    assert!(direct.contains("Ambiguous exact match"), "{direct}");
    assert_eq!(
        direct.matches("Retained patch retry: ").count(),
        1,
        "{direct}"
    );
    assert!(direct.contains("patch-1"), "{direct}");
}

#[test]
fn skipped_function_outputs_remain_typed_and_non_successful() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let neutral = FunctionToolOutput::from_text("not run".to_string(), Some(true))
        .with_outcome(ToolOutputOutcome::Skipped);
    assert_eq!(
        neutral.outcome_context(),
        ToolOutputOutcomeContext::skipped(None)
    );
    assert!(!neutral.success_for_logging());
    match neutral.to_response_item("skip-neutral", &payload) {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            assert_eq!(output.success, Some(false));
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }

    let deferred = FunctionToolOutput::from_text("later".to_string(), Some(true))
        .with_skip_disposition(ToolOutputSkipDisposition::Deferred);
    assert_eq!(
        deferred.outcome_context(),
        ToolOutputOutcomeContext::skipped(Some(ToolOutputSkipDisposition::Deferred))
    );
    assert!(!deferred.success_for_logging());
}

#[test]
fn mcp_code_mode_result_serializes_full_call_tool_result() {
    let output = CallToolResult {
        content: vec![serde_json::json!({
            "type": "text",
            "text": "ignored",
        })],
        structured_content: Some(serde_json::json!({
            "threadId": "thread_123",
            "content": "done",
        })),
        is_error: Some(false),
        meta: Some(serde_json::json!({
            "source": "mcp",
        })),
    };

    let result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });

    assert_eq!(
        result,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": "ignored",
            }],
            "structuredContent": {
                "threadId": "thread_123",
                "content": "done",
            },
            "isError": false,
            "_meta": {
                "source": "mcp",
            },
        })
    );
}

fn assert_mcp_wrapper_preserves_projection_metadata(
    result: CallToolResult,
    expected_outcome: ToolOutputOutcome,
    expected_diagnostic_class: ToolOutputDiagnosticClass,
) {
    let native = ToolOutput::projection_metadata(&result).expect("native MCP metadata");
    let wrapped = mcp_tool_output(
        result,
        std::time::Duration::from_millis(25),
        false,
        TruncationPolicy::Bytes(1024),
    )
    .projection_metadata()
    .expect("wrapped MCP metadata");

    assert_eq!(wrapped.outcome, expected_outcome);
    assert_eq!(wrapped.diagnostic_class, expected_diagnostic_class);
    assert_eq!(wrapped.outcome, native.outcome);
    assert_eq!(wrapped.diagnostic_class, native.diagnostic_class);
    assert_eq!(wrapped.fragments, native.fragments);
    assert_eq!(wrapped.spillable_text, native.spillable_text);
    assert_eq!(wrapped.essential_inline, native.essential_inline);
    assert_eq!(wrapped.requested_limit, native.requested_limit);
    assert_eq!(wrapped.predetermined_ranges, native.predetermined_ranges);
    assert_eq!(
        wrapped.predetermined_json_pointers,
        native.predetermined_json_pointers
    );

    let limits_for = |metadata: &ToolOutputProjectionMetadata| {
        let outcome = match metadata.outcome {
            ToolOutputOutcome::Success => OutputOutcome::Success,
            ToolOutputOutcome::Failure => OutputOutcome::Failure,
            ToolOutputOutcome::TimedOut => OutputOutcome::TimedOut,
            ToolOutputOutcome::Yielded => OutputOutcome::Success,
            ToolOutputOutcome::Skipped => OutputOutcome::Skipped,
        };
        let diagnostic_class = match metadata.diagnostic_class {
            ToolOutputDiagnosticClass::Normal => {
                codex_utils_output_truncation::OutputDiagnosticClass::Normal
            }
            ToolOutputDiagnosticClass::HighSignal => {
                codex_utils_output_truncation::OutputDiagnosticClass::HighSignal
            }
        };
        resolve_projected_output_limits(metadata.requested_limit, outcome, diagnostic_class, 4_000)
    };
    assert_eq!(limits_for(&wrapped), limits_for(&native));
}

#[test]
fn mcp_wrapper_preserves_native_success_projection_metadata() {
    assert_mcp_wrapper_preserves_projection_metadata(
        CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "provider success",
            })],
            structured_content: Some(serde_json::json!({"value": 42})),
            is_error: Some(false),
            meta: Some(serde_json::json!({"provider": "fixture"})),
        },
        ToolOutputOutcome::Success,
        ToolOutputDiagnosticClass::Normal,
    );
}

#[test]
fn mcp_wrapper_preserves_native_high_signal_projection_metadata() {
    let result = CallToolResult {
        content: vec![serde_json::json!({
            "type": "text",
            "text": "provider failure",
        })],
        structured_content: Some(serde_json::json!({"code": "provider_failed"})),
        is_error: Some(true),
        meta: Some(serde_json::json!({"provider": "fixture"})),
    };
    let native = ToolOutput::projection_metadata(&result).expect("native MCP metadata");
    assert_mcp_wrapper_preserves_projection_metadata(
        result,
        ToolOutputOutcome::Failure,
        ToolOutputDiagnosticClass::HighSignal,
    );
    assert!(
        native.fragments.iter().any(|fragment| {
            fragment.kind == ToolOutputProjectionFragmentKind::ErrorOrDiagnostic
        })
    );
    let applied = resolve_projected_output_limits(
        native.requested_limit,
        OutputOutcome::Failure,
        codex_utils_output_truncation::OutputDiagnosticClass::HighSignal,
        4_000,
    );
    assert_eq!(applied.applied_limit, 4_000);
}

#[test]
fn mcp_tool_output_response_item_includes_wall_time() {
    let output = mcp_tool_output(
        CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "done",
            })],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        },
        std::time::Duration::from_millis(1250),
        false,
        TruncationPolicy::Bytes(1024),
    );

    let response = output.to_response_item(
        "mcp-call-1",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );

    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "mcp-call-1");
            assert_eq!(output.success, Some(true));
            let Some(text) = output.body.to_text() else {
                panic!("MCP output should serialize as text");
            };
            let Some(payload) = text.strip_prefix("Wall time: 1.2500 seconds\nOutput:\n") else {
                panic!("MCP output should include wall-time header: {text}");
            };
            assert_eq!(payload, "done", "plain MCP text retains its content");
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn mcp_sampling_identity_excludes_wall_time() {
    let result = CallToolResult {
        content: vec![json!({ "type": "text", "text": "done" })],
        structured_content: None,
        is_error: Some(false),
        meta: None,
    };
    let output = |wall_time| {
        mcp_tool_output(
            result.clone(),
            wall_time,
            false,
            TruncationPolicy::Bytes(1024),
        )
    };

    assert_eq!(
        output(std::time::Duration::from_millis(1)).sampling_request_signal(),
        output(std::time::Duration::from_secs(9)).sampling_request_signal(),
    );
}

#[test]
fn confirmed_performance_mcp_output_reuses_raw_and_provider_projections() {
    let output = mcp_tool_output(
        CallToolResult {
            content: vec![json!({ "type": "text", "text": "done" })],
            structured_content: Some(json!({ "value": 42 })),
            is_error: Some(false),
            meta: None,
        },
        std::time::Duration::from_millis(25),
        false,
        TruncationPolicy::Bytes(1024),
    );
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };

    assert_eq!(output.projection_cache_state(), (false, false));
    let expected_raw = output.code_mode_result(&payload);
    assert_eq!(output.sampling_request_signal().is_some(), true);
    assert_eq!(
        output.post_tool_use_response("call", &payload),
        Some(expected_raw)
    );
    assert_eq!(output.projection_cache_state(), (true, false));

    let cloned = output.clone();
    let _ = output.log_preview();
    let _ = output.to_response_item("call", &payload);
    assert_eq!(output.projection_cache_state(), (true, true));
    assert_eq!(cloned.projection_cache_state(), (true, true));
}

#[test]
fn mcp_tool_output_response_item_truncates_large_structured_content() {
    let output = mcp_tool_output(
        CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "distinct caption",
            })],
            structured_content: Some(serde_json::json!({
                "items": "large structured value ".repeat(1_000),
            })),
            is_error: Some(false),
            meta: None,
        },
        std::time::Duration::from_millis(1250),
        false,
        TruncationPolicy::Bytes(128),
    );

    let response = output.to_response_item(
        "mcp-call-large",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );

    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "mcp-call-large");
            assert_eq!(output.success, Some(true));
            let text = output
                .body
                .to_text()
                .expect("MCP output should serialize as text");
            assert!(text.starts_with("Wall time: 1.2500 seconds\nOutput:\n"));
            assert!(text.contains("chars truncated"));
            assert!(text.contains("distinct caption"));
            assert!(text.contains("\"items\""));
            assert!(text.contains("large structured value"));
            assert!(
                text.len() < 512,
                "the large structured value must be bounded"
            );
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn mcp_tool_output_response_item_preserves_content_items() {
    let image_url = "data:image/png;base64,AAA";
    let output = mcp_tool_output(
        CallToolResult {
            content: vec![serde_json::json!({
                "type": "image",
                "mimeType": "image/png",
                "data": "AAA",
            })],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        },
        std::time::Duration::from_millis(500),
        false,
        TruncationPolicy::Bytes(1024),
    );

    let response = output.to_response_item(
        "mcp-call-2",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );

    match response {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            assert_eq!(
                output.content_items(),
                Some(
                    vec![
                        FunctionCallOutputContentItem::InputText {
                            text: "Wall time: 0.5000 seconds\nOutput:".to_string(),
                        },
                        FunctionCallOutputContentItem::InputImage {
                            image_url: image_url.to_string(),
                            detail: Some(DEFAULT_IMAGE_DETAIL),
                        },
                    ]
                    .as_slice()
                )
            );
            assert_eq!(
                output.body.to_text().as_deref(),
                Some("Wall time: 0.5000 seconds\nOutput:")
            );
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn mcp_tool_output_code_mode_result_stays_raw_call_tool_result() {
    let large_content = "large structured value ".repeat(1_000);
    let output = mcp_tool_output(
        CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "ignored",
            })],
            structured_content: Some(serde_json::json!({
                "content": large_content,
            })),
            is_error: Some(false),
            meta: None,
        },
        std::time::Duration::from_millis(1250),
        false,
        TruncationPolicy::Bytes(64),
    );

    let result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });

    assert_eq!(
        result,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": "ignored",
            }],
            "structuredContent": {
                "content": "large structured value ".repeat(1_000),
            },
            "isError": false,
        })
    );
}

#[test]
fn custom_tool_calls_can_derive_text_from_content_items() {
    let payload = ToolPayload::Custom {
        input: "patch".to_string(),
    };
    let response = FunctionToolOutput::from_content(
        vec![
            FunctionCallOutputContentItem::InputText {
                text: "line 1".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAA".to_string(),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            FunctionCallOutputContentItem::InputText {
                text: "line 2".to_string(),
            },
        ],
        Some(true),
    )
    .to_response_item("call-99", &payload);

    match response {
        ResponseInputItem::CustomToolCallOutput {
            call_id, output, ..
        } => {
            let expected = vec![
                FunctionCallOutputContentItem::InputText {
                    text: "line 1".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAA".to_string(),
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "line 2".to_string(),
                },
            ];
            assert_eq!(call_id, "call-99");
            assert_eq!(output.content_items(), Some(expected.as_slice()));
            assert_eq!(output.body.to_text().as_deref(), Some("line 1\nline 2"));
            assert_eq!(output.success, Some(true));
        }
        other => panic!("expected CustomToolCallOutput, got {other:?}"),
    }
}

#[test]
fn function_output_with_image_uses_complete_json_canonical_result() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let output = FunctionToolOutput::from_content(
        vec![
            FunctionCallOutputContentItem::InputText {
                text: "caption".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAA".to_string(),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ],
        Some(true),
    );

    let canonical = output
        .canonical_result(&payload)
        .expect("canonical function output");
    let value: serde_json::Value =
        serde_json::from_slice(&canonical.bytes).expect("canonical JSON");

    assert!(value.to_string().contains("data:image/png;base64,AAA"));
    assert!(canonical.complete);
}

#[test]
fn confirmed_performance_single_text_function_output_uses_direct_canonical_text() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let output = FunctionToolOutput::from_text("exact text".to_string(), Some(true));
    FunctionToolOutput::reset_projection_metadata_call_count();

    let canonical = output
        .canonical_result(&payload)
        .expect("canonical function output");

    assert_eq!(canonical.bytes, b"exact text".to_vec());
    assert!(canonical.complete);
    assert_eq!(FunctionToolOutput::projection_metadata_call_count(), 0);
}

#[test]
fn multi_text_command_output_retains_line_addressable_canonical_bytes() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let diagnostic = "compiler output\nerror[E0308]: late decisive diagnostic";
    let receipt = "Nested command states:\n{\"exit_code\":1}";
    let output = FunctionToolOutput::from_content(
        vec![
            FunctionCallOutputContentItem::InputText {
                text: diagnostic.to_string(),
            },
            FunctionCallOutputContentItem::InputText {
                text: receipt.to_string(),
            },
        ],
        Some(false),
    );
    let canonical = output.canonical_result(&payload).unwrap();
    let text = String::from_utf8(canonical.bytes).unwrap();
    assert_eq!(text, format!("{diagnostic}\n{receipt}"));
    assert_eq!(
        text.lines().nth(1),
        Some("error[E0308]: late decisive diagnostic")
    );
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    let whitespace = FunctionToolOutput::from_text(" \n\t".to_string(), Some(true));
    assert_eq!(
        whitespace.canonical_result(&payload).unwrap().bytes,
        b" \n\t"
    );
}

#[test]
fn tool_search_payloads_roundtrip_as_tool_search_outputs() {
    let payload = ToolPayload::ToolSearch {
        arguments: SearchToolCallParams {
            query: "calendar".to_string(),
            limit: None,
        },
    };
    let output = ToolSearchOutput {
        tools: vec![json!({
            "type": "function",
            "name": "create_event",
            "description": "",
            "strict": false,
            "defer_loading": true,
            "parameters": {
                "type": "object",
                "properties": {}
            }
        })],
        omitted_result_count: 0,
        activated_omitted_tools: Vec::new(),
        unactivated_matches: Vec::new(),
        unmatched_identifiers: Vec::new(),
        exact_name_ambiguity: None,
    };
    assert_eq!(
        output.code_mode_result(&payload),
        json!({
            "status": "completed",
            "execution": "client",
            "tools": [{
                "type": "function",
                "name": "create_event",
                "description": "",
                "strict": false,
                "defer_loading": true,
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }],
            "omitted_result_count": 0,
        })
    );
    let response = output.to_response_item("search-1", &payload);

    match response {
        ResponseInputItem::ToolSearchOutput {
            call_id,
            status,
            execution,
            tools,
            omitted_result_count,
        } => {
            assert_eq!(call_id, "search-1");
            assert_eq!(status, "completed");
            assert_eq!(execution, "client");
            assert_eq!(omitted_result_count, Some(0));
            assert_eq!(
                tools,
                vec![json!({
                    "type": "function",
                    "name": "create_event",
                    "description": "",
                    "strict": false,
                    "defer_loading": true,
                    "parameters": {
                        "type": "object",
                        "properties": {}
                    }
                })]
            );
        }
        other => panic!("expected ToolSearchOutput, got {other:?}"),
    }
}

#[test]
fn partial_tool_search_outputs_are_model_visible_as_incomplete() {
    let payload = ToolPayload::ToolSearch {
        arguments: SearchToolCallParams {
            query: "calendar".to_string(),
            limit: None,
        },
    };
    let output = ToolSearchOutput {
        tools: Vec::new(),
        omitted_result_count: 1,
        activated_omitted_tools: vec!["calendar.create_event".to_string()],
        unactivated_matches: Vec::new(),
        unmatched_identifiers: Vec::new(),
        exact_name_ambiguity: None,
    };
    assert_eq!(
        output.code_mode_result(&payload),
        json!({
            "status": "incomplete",
            "execution": "client",
            "tools": [],
            "omitted_result_count": 1,
            "activated_omitted_tools": ["calendar.create_event"],
            "resolution": {"helper":"resolve_tool", "argument":"exact activated_omitted_tools name"},
        })
    );
    let response = output.to_response_item("search-partial", &payload);

    match response {
        ResponseInputItem::ToolSearchOutput {
            status,
            tools,
            omitted_result_count,
            ..
        } => {
            assert_eq!(status, "incomplete");
            assert!(tools.is_empty());
            assert_eq!(omitted_result_count, Some(1));
        }
        other => panic!("expected ToolSearchOutput, got {other:?}"),
    }
}

#[test]
fn aborted_tool_search_payloads_preserve_abort_status() {
    let payload = ToolPayload::ToolSearch {
        arguments: SearchToolCallParams {
            query: "calendar".to_string(),
            limit: None,
        },
    };

    let output = AbortedToolOutput {
        message: "cancelled".to_string(),
    };
    assert_eq!(
        output.code_mode_result(&payload),
        json!({
            "status": "aborted",
            "execution": "client",
            "tools": [],
            "omitted_result_count": null,
        })
    );
    assert_eq!(
        output.to_response_item("search-aborted", &payload),
        ResponseInputItem::ToolSearchOutput {
            call_id: "search-aborted".to_string(),
            status: "aborted".to_string(),
            execution: "client".to_string(),
            tools: Vec::new(),
            omitted_result_count: None,
        }
    );
}

#[test]
fn verified10_search_receipts_match_advertised_schema() {
    let schema = codex_tools::code_mode_tool_search_output_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let payload = ToolPayload::ToolSearch {
        arguments: SearchToolCallParams { query: "messages".into(), limit: None },
    };
    let mut output = ToolSearchOutput {
        tools: vec![], omitted_result_count: 0, activated_omitted_tools: vec![],
        unactivated_matches: vec![], unmatched_identifiers: vec![], exact_name_ambiguity: None,
    };
    validator.validate(&output.code_mode_result(&payload)).unwrap();
    output.omitted_result_count = 1;
    output.activated_omitted_tools.push("mail.messages".into());
    validator.validate(&output.code_mode_result(&payload)).unwrap();
    output.exact_name_ambiguity = Some(json!({
        "match_count": 2, "omitted_alternative_count": 0,
        "qualified_alternatives": ["mail.messages", "chat.messages"]
    }));
    output.unmatched_identifiers.push("project_alpha".into());
    validator.validate(&output.code_mode_result(&payload)).unwrap();
    validator.validate(&AbortedToolOutput { message: "cancelled".into() }.code_mode_result(&payload)).unwrap();
    let mut invalid = output.code_mode_result(&payload);
    invalid["resolution"]["helper"] = json!("invented");
    assert!(!validator.is_valid(&invalid));
}

#[test]
fn verified10_search_rejects_invented_filters_with_scope_guidance() {
    for field in ["source", "provider", "namespace"] {
        let error = serde_json::from_value::<SearchToolCallParams>(json!({"query":"messages", (field):"gmail"})).unwrap_err().to_string();
        assert!(error.contains("unknown field"), "{error}");
        assert!(error.contains("source:<canonical namespace>"), "{error}");
    }
    let args: SearchToolCallParams = serde_json::from_value(json!({"query":"messages source:gmail", "limit":1})).unwrap();
    assert_eq!(args.limit, Some(1));
}

#[test]
fn verified10_anchor_survives_actual_input_normalization() {
    let input = json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object", "properties":{"duration":{"$ref":"#Bounded"}},
        "$defs":{"duration":{"$anchor":"Bounded", "type":"number", "minimum":1, "maximum":5}}
    });
    let normalized = serde_json::to_value(codex_tools::parse_tool_input_schema(&input).unwrap()).unwrap();
    assert_eq!(normalized["$defs"]["duration"]["$anchor"], "Bounded");
    let before = jsonschema::validator_for(&input).unwrap();
    let after = jsonschema::validator_for(&normalized).unwrap();
    for (value, valid) in [(json!({"duration":3}), true), (json!({"duration":6}), false), (json!({"duration":"3"}), false)] {
        assert_eq!(before.is_valid(&value), valid);
        assert_eq!(after.is_valid(&value), valid);
    }
    for keyword in ["$dynamicAnchor", "$recursiveAnchor"] {
        assert!(codex_tools::parse_tool_input_schema(&json!({"type":"object", (keyword):"Bounded"})).unwrap_err().to_string().contains(keyword));
    }
}

#[test]
fn ordinary_aborted_code_mode_output_is_structured() {
    let output = AbortedToolOutput {
        message: "cancelled".to_string(),
    };

    assert_eq!(
        output.code_mode_result(&ToolPayload::Function {
            arguments: "{}".to_string(),
        }),
        json!({
            "status": "aborted",
            "message": "cancelled",
        })
    );
}

#[test]
fn log_preview_uses_content_items_when_plain_text_is_missing() {
    let output = FunctionToolOutput::from_content(
        vec![FunctionCallOutputContentItem::InputText {
            text: "preview".to_string(),
        }],
        Some(true),
    );

    assert_eq!(output.log_preview(), "preview");
    assert_eq!(
        function_call_output_content_items_to_text(&output.body),
        Some("preview".to_string())
    );
}

#[test]
fn command_semantic_evidence_normalizes_read_only_presentations() {
    let source_fact = "let stable = compute();";
    let presentations = [
        source_fact.to_string(),
        format!("src/lib.rs:10:{source_fact}"),
        format!("README.md:494:{source_fact}"),
        format!("diff --git a/src/lib.rs b/src/lib.rs\n@@ -9,0 +10 @@\n+{source_fact}"),
        format!("  --> src/lib.rs:10:1\n10 | {source_fact}\n   | ^^^"),
    ];
    let expected = semantic_evidence_for_command_output(presentations[0].as_bytes());
    for presentation in &presentations[1..] {
        assert_eq!(
            semantic_evidence_for_command_output(presentation.as_bytes()),
            expected
        );
        assert_eq!(
            command_failure_signature(
                &semantic_evidence_for_command_output(presentation.as_bytes()),
                Some(1)
            ),
            command_failure_signature(&expected, Some(1))
        );
    }
    assert_ne!(
        semantic_evidence_for_command_output(b"let changed = compute();"),
        expected
    );
}

#[test]
fn command_semantic_evidence_preserves_diagnostics_and_non_location_numbers() {
    let source = "10 | let stable = compute();";
    let first_diagnostic = format!("error[E0001]: first failure\n{source}");
    let second_diagnostic = format!("error[E0002]: second failure\n{source}");
    assert_ne!(
        semantic_evidence_for_command_output(first_diagnostic.as_bytes()),
        semantic_evidence_for_command_output(second_diagnostic.as_bytes())
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"service-a:8080: healthy"),
        semantic_evidence_for_command_output(b"service-b:9090: healthy")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"12 failures remain"),
        semantic_evidence_for_command_output(b"13 failures remain")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"https://service-a:8080: healthy"),
        semantic_evidence_for_command_output(b"https://service-b:8080: healthy")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"db.example.com:5432: ready"),
        semantic_evidence_for_command_output(b"cache.example.com:5432: ready")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"src/lib.rs:10:8080: healthy"),
        semantic_evidence_for_command_output(b"src/lib.rs:10:9090: healthy")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"running 5 workers"),
        semantic_evidence_for_command_output(b"running 6 workers")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"let value = \"a  b\";"),
        semantic_evidence_for_command_output(b"let value = \"a b\";")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"10 | legitimate table value"),
        semantic_evidence_for_command_output(b"legitimate table value")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"fact\n}"),
        semantic_evidence_for_command_output(b"fact\n]")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"first fact\nsecond fact"),
        semantic_evidence_for_command_output(b"second fact\nfirst fact")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"Ok"),
        semantic_evidence_for_command_output(b"No")
    );
    assert_ne!(
        semantic_evidence_for_command_output(&[0xff, b'a']),
        semantic_evidence_for_command_output("�a".as_bytes())
    );
    assert_ne!(
        semantic_evidence_for_command_output(
            b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -9,0 +10 @@\n+same fact\nfatal: first"
        ),
        semantic_evidence_for_command_output(
            b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -9,0 +10 @@\n+same fact\nfatal: second"
        )
    );
}

#[test]
fn command_semantic_evidence_preserves_removed_and_context_diff_lines() {
    let first_removal =
        b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-old value\n+new value";
    let second_removal =
        b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-other old value\n+new value";
    assert_ne!(
        semantic_evidence_for_command_output(first_removal),
        semantic_evidence_for_command_output(second_removal)
    );

    let first_context = b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -1,2 +1,2 @@\n first context\n-old value\n+new value";
    let second_context = b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -1,2 +1,2 @@\n second context\n-old value\n+new value";
    assert_ne!(
        semantic_evidence_for_command_output(first_context),
        semantic_evidence_for_command_output(second_context)
    );
}

#[test]
fn ansi_stripping_preserves_text_after_a_non_csi_escape() {
    assert_eq!(strip_ansi_sequences("before\u{1b}Xafter"), "beforeXafter");
    assert_eq!(
        strip_ansi_sequences("before\u{1b}[31mred\u{1b}[0mafter"),
        "beforeredafter"
    );
}

#[test]
fn command_semantic_evidence_includes_facts_after_the_old_limit() {
    let shared = (0..512)
        .map(|index| format!("shared fact {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let first = format!("{shared}\nfirst tail fact");
    let second = format!("{shared}\nsecond tail fact");
    assert_ne!(
        semantic_evidence_for_command_output(first.as_bytes()),
        semantic_evidence_for_command_output(second.as_bytes())
    );

    let long_prefix = "x".repeat(4_096);
    assert_ne!(
        semantic_evidence_for_command_output(format!("{long_prefix} first").as_bytes()),
        semantic_evidence_for_command_output(format!("{long_prefix} second").as_bytes())
    );
}

#[test]
fn command_semantic_evidence_preserves_fact_multiplicity() {
    assert_ne!(
        semantic_evidence_for_command_output(b"same fact\nsame fact"),
        semantic_evidence_for_command_output(b"same fact")
    );
}

#[test]
fn command_failure_signature_preserves_exit_status() {
    let evidence = semantic_evidence_for_command_output(b"same diagnostic");
    assert_ne!(
        command_failure_signature(&evidence, Some(1)),
        command_failure_signature(&evidence, Some(2))
    );
}

#[test]
fn token_efficiency_exec_output_omits_redundant_headers() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let response = ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "call-42".to_string(),
        chunk_id: "abc123".to_string(),
        wall_time: std::time::Duration::from_millis(1250),
        raw_output: vec![b'x'; 400],
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(20),
        process_id: None,
        session_capabilities: None,
        exit_code: Some(0),
        process_exited: true,
        search_no_match: false,
        original_token_count: Some(100),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    }
    .to_response_item("call-42", &payload);

    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "call-42");
            assert_eq!(output.success, Some(true));
            let text = output
                .body
                .to_text()
                .expect("exec output should serialize as text");
            let packet: JsonValue = serde_json::from_str(&text).unwrap();
            assert_eq!(packet["exit_code"], 0);
            assert!(
                packet["output"].as_str().unwrap().contains("\n[...]\n"),
                "{text}"
            );
            assert!(!text.contains("Chunk ID:"));
            assert!(!text.contains("Original token count:"));
            assert!(
                codex_utils_string::approx_token_count(packet["output"].as_str().unwrap()) <= 20
            );
            assert_ne!(
                text,
                String::from_utf8(vec![b'x'; 400]).expect("UTF-8 fixture")
            );
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn retained_exec_command_process_is_yielded_not_timed_out() {
    let output = ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "retained-call".to_string(),
        chunk_id: "retained-chunk".to_string(),
        wall_time: std::time::Duration::from_millis(250),
        raw_output: b"process still running".to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: None,
        process_id: Some(4242),
        session_capabilities: None,
        exit_code: None,
        process_exited: false,
        search_no_match: false,
        original_token_count: Some(3),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Yielded);
    assert!(!output.success_for_logging());
    assert_eq!(
        output.model_output_max_tokens(),
        codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS
    );
    assert_eq!(
        output
            .projection_metadata()
            .expect("retained output should have projection metadata")
            .outcome,
        ToolOutputOutcome::Yielded
    );
    match output.to_response_item(
        "retained-call",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    ) {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            assert_eq!(output.success, Some(false));
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn tool_result_correctness_missing_exit_code_is_not_reported_as_success() {
    let output = ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "missing-exit-call".to_string(),
        chunk_id: "missing-exit-chunk".to_string(),
        wall_time: std::time::Duration::from_millis(10),
        raw_output: b"exit status unavailable".to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(1_000),
        process_id: None,
        session_capabilities: None,
        exit_code: None,
        process_exited: true,
        search_no_match: false,
        original_token_count: Some(3),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    assert!(!output.success_for_logging());
    assert!(
        output
            .response_text()
            .contains("Process exited without an available exit code")
    );
    let code_mode = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    assert_eq!(code_mode["process_exited"], json!(true));
    assert_eq!(code_mode["exit_code"], JsonValue::Null);
}

#[test]
fn exec_output_discloses_lossy_decoding_without_changing_canonical_bytes() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let mut output = ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "decoding-call".to_string(),
        chunk_id: "decoding-chunk".to_string(),
        wall_time: std::time::Duration::from_millis(10),
        raw_output: b"invalid: \xff".to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(1_000),
        process_id: None,
        session_capabilities: None,
        exit_code: Some(1),
        process_exited: true,
        search_no_match: false,
        original_token_count: None,
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };
    let expected_notice = "Output contained invalid UTF-8 bytes, which were replaced with U+FFFD. The displayed text is not byte-exact.";
    let ResponseInputItem::FunctionCallOutput {
        output: response, ..
    } = output.to_response_item("decoding-call", &payload)
    else {
        panic!("function output")
    };
    let text = response.body.to_text().expect("text output");
    assert!(text.contains(expected_notice));
    assert!(text.contains("invalid: \u{fffd}"));
    let code_mode = output.code_mode_result(&payload);
    assert_eq!(code_mode["output_decoding_notice"], expected_notice);
    assert_eq!(code_mode["exit_code"], 1);
    assert!(
        code_mode
            .get("original_token_count_is_approximate")
            .is_none()
    );
    assert!(
        output
            .projection_metadata()
            .unwrap()
            .essential_inline
            .get("original_token_count_is_approximate")
            .is_none()
    );
    assert_eq!(
        output.projection_metadata().unwrap().essential_inline["output_decoding_notice"],
        expected_notice
    );
    assert_eq!(
        output.canonical_result(&payload).unwrap().bytes,
        b"invalid: \xff"
    );

    output.raw_output = b"first\r\nsecond\r\n".to_vec();
    assert_eq!(output.code_mode_result(&payload)["output"], "first\nsecond\n");
    assert_eq!(output.projection_metadata().unwrap().spillable_text, ["first\nsecond\n"]);
    assert_eq!(output.canonical_result(&payload).unwrap().bytes, b"first\r\nsecond\r\n");
    output.raw_output = "word ".repeat(30_000).into_bytes();
    output.max_output_tokens = Some(25_000);
    assert_eq!(output.model_output_limits("", None).applied_limit, 10_000);
    assert_eq!(output.model_output_limits("", Some(8_000)).applied_limit, 8_000);

    // A literal replacement character in valid UTF-8 is not evidence of loss.
    output.raw_output = "valid: \u{fffd}".as_bytes().to_vec();
    assert!(
        output
            .code_mode_result(&payload)
            .get("output_decoding_notice")
            .is_none()
    );
    assert!(
        output
            .projection_metadata()
            .unwrap()
            .essential_inline
            .get("output_decoding_notice")
            .is_none()
    );
}

#[test]
fn tool_result_correctness_exited_process_with_pending_output_is_not_live() {
    let output = ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "pending-output-call".to_string(),
        chunk_id: "pending-output-chunk".to_string(),
        wall_time: std::time::Duration::from_millis(10),
        raw_output: b"remaining output".to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(1_000),
        process_id: Some(4242),
        session_capabilities: None,
        exit_code: Some(7),
        process_exited: true,
        search_no_match: false,
        original_token_count: Some(2),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    let metadata = output
        .projection_metadata()
        .expect("exec output should expose projection metadata");
    assert_eq!(metadata.essential_inline["session_id"], json!(4242));
    assert_eq!(metadata.essential_inline["exit_code"], json!(7));
    assert_eq!(metadata.essential_inline["process_exited"], json!(true));
    let code_mode = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    assert_eq!(code_mode["process_exited"], json!(true));
    assert!(output.response_text().contains("\"exit_code\":7"));
    assert!(!output.response_text().contains("Process running"));
}

#[test]
fn exec_command_projection_metadata_preserves_authoritative_first_output() {
    let raw_output = "first output line\n".repeat(100);
    let output = ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "call-first-output".to_string(),
        chunk_id: "chunk-first-output".to_string(),
        wall_time: std::time::Duration::from_millis(1),
        raw_output: raw_output.as_bytes().to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(20),
        process_id: Some(42),
        session_capabilities: None,
        exit_code: None,
        process_exited: false,
        search_no_match: false,
        original_token_count: Some(300),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };

    let metadata = output
        .projection_metadata()
        .expect("exec output should expose projection metadata");

    assert_eq!(metadata.spillable_text, vec![raw_output.clone()]);
    assert_eq!(
        metadata.fragments,
        vec![
            ToolOutputProjectionFragment::new(
                ToolOutputProjectionFragmentKind::ProcessFinalStatus,
                "process final status: exit_code=None, session_id=Some(42)",
            )
            .with_id("process_status"),
            ToolOutputProjectionFragment::new(
                ToolOutputProjectionFragmentKind::ContextualSpillableText,
                raw_output,
            )
            .with_id("output")
        ]
    );
    assert_eq!(metadata.essential_inline["session_id"], json!(42));
    assert_eq!(metadata.essential_inline["exit_code"], JsonValue::Null);
    assert_eq!(metadata.essential_inline["wall_time_seconds"], json!(0.001));
    assert_eq!(metadata.essential_inline["repair_notice"], JsonValue::Null);
}

#[test]
fn token_efficiency_exec_projection_reports_truncation_once() {
    let output = ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "call-hard-limit".to_string(),
        chunk_id: "chunk-hard-limit".to_string(),
        wall_time: std::time::Duration::from_millis(1),
        raw_output: vec![b'x'; 400],
        truncation_policy: TruncationPolicy::Tokens(5),
        max_output_tokens: Some(20),
        process_id: None,
        session_capabilities: None,
        exit_code: Some(0),
        process_exited: true,
        search_no_match: false,
        original_token_count: Some(100),
        hook_command: Some("echo ok".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };

    let raw_output = String::from_utf8_lossy(&output.raw_output);
    let projected = output.projected_model_output(raw_output.as_ref(), None);
    assert!(projected.reduced);
    assert_eq!(
        projected.text.matches('…').count(),
        1,
        "{}",
        projected.text
    );
    assert!(codex_utils_string::approx_token_count(&projected.text) <= 5);
    assert!(!projected.text.contains("tokens truncated"));
}

#[test]
fn exec_command_projection_reports_reduction_from_per_call_limit() {
    let output = ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "call-per-call-limit".to_string(),
        chunk_id: "chunk-per-call-limit".to_string(),
        wall_time: std::time::Duration::from_millis(1),
        raw_output: b"token one token two token three token four token five".to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(4),
        process_id: None,
        session_capabilities: None,
        exit_code: Some(0),
        process_exited: true,
        search_no_match: false,
        original_token_count: Some(10),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };

    let raw_output = String::from_utf8_lossy(&output.raw_output);
    let projected = output.projected_model_output(raw_output.as_ref(), None);
    assert!(projected.reduced);
    assert!(!projected.text.is_empty());
    assert!(codex_utils_string::approx_token_count(&projected.text) <= 4);
}

#[test]
fn token_backfire_unified_exec_keeps_complete_output_that_fits_budget() {
    let raw_output = (0..700)
        .map(|index| format!("line-{index}: exact evidence"))
        .collect::<Vec<_>>()
        .join("\n");
    let output = ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "call-complete-output".to_string(),
        chunk_id: "chunk-complete-output".to_string(),
        wall_time: std::time::Duration::from_millis(1),
        raw_output: raw_output.as_bytes().to_vec(),
        truncation_policy: TruncationPolicy::Tokens(20_000),
        max_output_tokens: Some(20_000),
        process_id: None,
        session_capabilities: None,
        exit_code: Some(0),
        process_exited: true,
        search_no_match: false,
        original_token_count: Some(codex_utils_string::approx_token_count(&raw_output)),
        hook_command: Some("enumerate evidence".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };

    let projected = output.projected_model_output(&raw_output, None);

    assert!(!projected.reduced);
    assert_eq!(projected.text, raw_output);
}

#[test]
fn high_signal_validation_exposes_failure_anchored_predetermined_range() {
    let raw_output = (1..=300)
        .map(|line| format!("error[E0001]: focused diagnostic line {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    let ranges = predetermined_validation_ranges(&raw_output, Some("cargo test -p focused"));

    assert_eq!(
        ranges,
        vec![
            ToolOutputProjectionRange {
                id: "validation:diagnostics".to_string(),
                start_line: 1,
                end_line: 200,
            },
        ]
    );
    assert_eq!(
        ranges
            .iter()
            .map(|range| range.end_line - range.start_line + 1)
            .sum::<usize>(),
        200
    );
}

#[test]
fn predetermined_validation_ranges_are_absent_when_not_needed() {
    let ordinary = (1..=300)
        .map(|line| format!("ordinary output {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(predetermined_validation_ranges(&ordinary, Some("echo ok")).is_empty());

    let short_diagnostic = (1..=200)
        .map(|line| format!("error: focused diagnostic {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        predetermined_validation_ranges(&short_diagnostic, Some("cargo check -p focused"))
            .is_empty()
    );
}

#[test]
fn token_efficiency_exec_output_preserves_live_process_state_for_large_output() {
    let raw_output = (0..900)
        .map(|index| format!("live-process-output-{index:04}-{}", "x".repeat(72)))
        .collect::<Vec<_>>()
        .join("\n");
    let response = ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "call-live".to_string(),
        chunk_id: "chunk-live".to_string(),
        wall_time: std::time::Duration::from_millis(25),
        raw_output: raw_output.as_bytes().to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(256),
        process_id: Some(42),
        session_capabilities: None,
        exit_code: None,
        process_exited: false,
        search_no_match: false,
        original_token_count: Some(20_000),
        hook_command: Some("cargo test".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    }
    .response_text();

    assert!(response.contains("\"session_id\":42"));
    assert!(!response.contains("Process exited with code"));
    assert!(!response.contains("exit_code: 0"));
    assert!(!response.contains("timed_out: true"));
    assert_eq!(response.matches("Warning: truncated output").count(), 1);
    assert!(response.len() < raw_output.len());
}

#[test]
fn exec_command_tool_output_summarizes_and_links_retained_raw_output() {
    let raw_output = (0..900)
        .map(|index| {
            if index == 450 {
                format!("error: exact retained failure marker {index}")
            } else {
                format!("ordinary-{index:04}-{}", "x".repeat(72))
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let artifact_id: ToolOutputArtifactId = "019fa782-f8e1-7533-a3f7-60d3f9a42997".parse().unwrap();
    let artifact_path =
        std::path::PathBuf::from(format!(r"C:\codex\tool-output\{artifact_id}.log"));
    let mut output = ExecCommandToolOutput {
        output_ranges: None,
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "call-summary".to_string(),
        chunk_id: "chunk-summary".to_string(),
        wall_time: std::time::Duration::from_millis(25),
        raw_output: raw_output.as_bytes().to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(10_000),
        process_id: None,
        session_capabilities: None,
        exit_code: Some(1),
        process_exited: true,
        search_no_match: false,
        original_token_count: Some(20_000),
        hook_command: Some("cargo test".to_string()),
        raw_output_artifact: Some(RawOutputArtifact::Stored {
            id: artifact_id,
            path: artifact_path.clone(),
            bytes: raw_output.len() as u64,
            truncated: false,
            handle: std::sync::Arc::new(tempfile::tempfile().expect("artifact handle")),
        }),
        repair_notice: Some("Command preflight applied one repair".to_string()),
        pending_deferred_completions: Vec::new(),
    };

    let response = output.response_text();
    assert!(
        codex_utils_output_truncation::approx_token_count(&response)
            <= output.model_output_max_tokens()
    );
    assert!(response.contains("Shell output summary:"));
    assert!(response.contains("error: exact retained failure marker 450"));
    assert!(!response.contains("ordinary-0300"));
    assert!(response.contains(&artifact_id.to_string()));
    assert!(!response.contains(&artifact_path.display().to_string()));
    assert!(response.contains("Command preflight applied one repair"));

    let code_mode = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    assert_eq!(code_mode["raw_output_artifact_id"], artifact_id.to_string());
    assert_eq!(code_mode["raw_output_artifact_bytes"], raw_output.len());
    assert_eq!(code_mode["original_token_count"], 20_000);
    assert_eq!(code_mode["original_token_count_is_approximate"], true);
    let essential = output
        .projection_metadata()
        .expect("exec projection")
        .essential_inline;
    assert_eq!(essential["original_token_count"], 20_000);
    assert_eq!(essential["original_token_count_is_approximate"], true);
    assert!(
        code_mode["output"]
            .as_str()
            .is_some_and(|value| value.contains("Shell output summary:"))
    );

    let summary = summarize_shell_output_for_model(
        &raw_output,
        1,
        false,
        ShellOutputSummaryOptions {
            enabled: true,
            applied_token_limit: None,
            command_text: Some("cargo test"),
        },
    )
    .expect("large output should summarize");
    let summary_tokens = codex_utils_string::approx_token_count(&summary);
    output.max_output_tokens = Some(summary_tokens);
    let tight = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    let tight_output = tight["output"].as_str().expect("projected output");
    assert_eq!(
        tight_output, summary,
        "a notice must not replace a summary that fits"
    );
    assert!(tight_output.contains("error: exact retained failure marker 450"));
    assert!(codex_utils_string::approx_token_count(tight_output) <= summary_tokens);
}

async fn artifact_backed_exec_output(
    raw_output: &[u8],
    max_output_tokens: Option<usize>,
) -> (
    ExecCommandToolOutput,
    ToolOutputArtifactId,
    std::path::PathBuf,
    tempfile::TempDir,
) {
    let retained_root = tempfile::tempdir().expect("retained artifact root");
    let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
        retained_root.path(),
        "thread",
        raw_output,
    )
    .await;
    let artifact_id = artifact.artifact_id().expect("normal artifact creation");
    let artifact_path = retained_root
        .path()
        .join("tool-output/thread")
        .join(format!("{artifact_id}.log"));
    let output = ExecCommandToolOutput {
        output_ranges: Some(crate::unified_exec::head_tail_buffer::OutputChunkRanges {
            range: 0..raw_output.len() as u64, gap: None,
        }),
        process_output: None,
        error: None,
        validation: None,
        event_call_id: "call-artifact".to_string(),
        chunk_id: "chunk-artifact".to_string(),
        wall_time: std::time::Duration::from_millis(1),
        raw_output: raw_output.to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens,
        process_id: None,
        session_capabilities: None,
        exit_code: Some(0),
        process_exited: true,
        search_no_match: false,
        original_token_count: None,
        hook_command: None,
        raw_output_artifact: Some(artifact),
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };
    (output, artifact_id, artifact_path, retained_root)
}

#[tokio::test]
async fn command_recovery_targets_current_chunk_gaps_not_cumulative_prefix() {
    use crate::unified_exec::head_tail_buffer::OutputChunkRanges;
    let prefix = b"already observed\r\n";
    let chunk = (0..500).map(|index| format!("current line {index:04}: exact λ evidence\r\n"))
        .collect::<String>();
    let raw = [prefix.as_slice(), chunk.as_bytes()].concat();
    let (mut output, id, _, root) = artifact_backed_exec_output(&raw, Some(80)).await;
    output.raw_output = chunk.as_bytes().to_vec();
    output.output_ranges = Some(OutputChunkRanges { range: prefix.len() as u64..raw.len() as u64, gap: None });
    let payload = ToolPayload::Function { arguments: "{}".into() };
    let packet = output.code_mode_result(&payload);
    let selector = &packet["recovery_selector"];
    assert_eq!(selector["kind"], "bytes");
    let start = selector["start"].as_u64().unwrap() as usize;
    let end = selector["end"].as_u64().unwrap() as usize;
    assert!(start >= prefix.len() && start < end && end <= raw.len());
    assert!(end - start > 4096, "the hint must not clip the known missing range");
    let retained = String::from_utf8_lossy(&raw[start..end]);
    assert!(!retained.contains("already observed"));
    assert!(retained.starts_with("current line"));
    assert_eq!(packet["recovery"]["arguments"]["selectors"][0], *selector);

    // One recovery request can now deliver this entire gap, without guessing
    // the next 4 KiB window or rereading the already observed cumulative prefix.
    let recovered = crate::tools::handlers::execute_recovery_transaction_with_continuations(
        root.path(), "thread", &id.to_string(),
        vec![serde_json::from_value(selector.clone()).unwrap()], true,
        &tokio_util::sync::CancellationToken::new(),
    ).await.unwrap().output;
    assert!(recovered.complete);
    let exact = recovered.results.iter().map(|result| result.text.as_deref().unwrap())
        .collect::<String>();
    assert_eq!(exact.as_bytes(), &raw[start..end]);

    // The first retention gap is known even if the displayed head/tail fit.
    output.raw_output = b"head\n[output retention gap]\ntail\n".to_vec();
    output.output_ranges = Some(OutputChunkRanges { range: 18..100, gap: Some(22..96) });
    output.max_output_tokens = Some(1000);
    let gap = output.code_mode_result(&payload);
    assert_eq!(gap["recovery_selector"], json!({"kind":"bytes", "start":22, "end":96}));
    assert_eq!(gap["output_reduced"], true);
    output.raw_output.clear();
    output.output_ranges = Some(OutputChunkRanges { range: raw.len() as u64..raw.len() as u64, gap: None });
    let empty = output.code_mode_result(&payload);
    assert!(empty.get("recovery_selector").is_none());
    assert_eq!(empty["output_reduced"], false);

    // Unknown coordinates must not turn into a guessed prefix selector.
    output.raw_output = chunk.into_bytes();
    output.output_ranges = None;
    output.max_output_tokens = Some(80);
    assert!(output.code_mode_result(&payload).get("recovery_selector").is_none());
}

#[tokio::test]
async fn command_recovery_preserves_gap_extent_and_retained_bounds() {
    use crate::unified_exec::head_tail_buffer::OutputChunkRanges;
    let raw = "exact λ evidence\r\n".repeat(1000);
    let (mut output, _, _, _root) = artifact_backed_exec_output(raw.as_bytes(), Some(80)).await;
    let retained = raw.len() as u64;
    output.raw_output = b"head\n[output retention gap]\ntail\n".to_vec();
    let payload = ToolPayload::Function { arguments: "{}".into() };
    for (gap, expected) in [
        (18..12_000, Some(json!({"kind":"bytes", "start":18, "end":12_000}))),
        (18..u64::MAX, Some(json!({"kind":"bytes", "start":18, "end":retained}))),
        (retained..u64::MAX, None),
        (retained + 1..u64::MAX, None),
    ] {
        output.output_ranges = Some(OutputChunkRanges { range: 0..u64::MAX, gap: Some(gap) });
        let result = output.code_mode_result(&payload);
        assert_eq!(result.get("recovery_selector"), expected.as_ref());
    }

    // Exact line-to-byte mapping for a later chunk must retain the requested
    // end, including CRLF and multibyte UTF-8, without a second size limit.
    output.raw_output = raw.as_bytes().to_vec();
    output.output_ranges = Some(OutputChunkRanges { range: 18..18 + retained, gap: None });
    let mapped = output.missing_output_selector(&raw, "", Some((2, 800)), Some(18 + retained));
    let line_bytes = "exact λ evidence\r\n".len() as u64;
    assert_eq!(mapped, Some(json!({
        "kind":"bytes", "start":18 + line_bytes, "end":18 + 800 * line_bytes,
    })));
}

#[tokio::test]
async fn exec_small_output_spills_only_when_its_display_is_reduced() {
    let raw = "retained producer bytes\n".repeat(256);
    assert!(raw.len() < crate::tools::command_output_artifact::LAZY_RAW_OUTPUT_ARTIFACT_THRESHOLD_BYTES);
    for pending in [false, true] {
        let (mut output, _, _, _root) = artifact_backed_exec_output(raw.as_bytes(), Some(10_000)).await;
        let home = tempfile::tempdir().unwrap();
        output.raw_output_artifact = pending.then(|| RawOutputArtifact::pending(home.path(), "thread"));
        output.prepare_recovery_artifact(home.path(), "thread").await;
        assert!(!home.path().join("tool-output").exists(), "inline output must not spill");

        output.max_output_tokens = Some(100);
        output.prepare_recovery_artifact(home.path(), "thread").await;
        let artifact_id = output.raw_output_artifact.as_ref().unwrap().artifact_id().unwrap();
        let result = output.code_mode_result(&ToolPayload::Function { arguments: "{}".into() });
        assert_eq!(result["output_reduced"], true);
        assert_eq!(result["raw_output_artifact_id"], artifact_id.to_string());
        let direct: serde_json::Value = serde_json::from_str(&output.response_text()).unwrap();
        assert_eq!(direct["artifact_id"], artifact_id.to_string());
        assert_eq!(std::fs::read(home.path().join("tool-output/thread").join(format!("{artifact_id}.log"))).unwrap(), raw.as_bytes());
        output.prepare_recovery_artifact(home.path(), "thread").await;
        assert_eq!(output.raw_output_artifact.as_ref().unwrap().artifact_id(), Some(artifact_id));
    }
}

#[test]
fn final_test_summaries_attribute_counts_without_execution_receipts() {
    for (command, output, expected) in [
        ("python -m unittest", "Ran 98 tests in 1.2s\n\nOK\n", Some(98)),
        ("python -m unittest", "Ran 4 tests in 1.2s\n\nOK (skipped=2)\n", Some(2)),
        ("pytest", "=== 12 passed, 2 skipped in 0.20s ===\n", Some(12)),
        ("cargo test", "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.0s\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.0s\n", Some(3)),
        ("cargo test", "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out; finished in 0.0s\n", None),
        ("python -m unittest", "Ran 4 tests in 1.2s\nOK (skipped=4)\n", None),
        ("python -m unittest", "Ran 4 tests in 1.2s\nFAILED (failures=1)\n", None),
        ("pytest", "2 failed, 12 passed in 0.20s", None),
        ("echo OK", "Ran 98 tests in 1.2s\nOK", None),
    ] {
        let validation = crate::validation::CommandValidation {
            execution_context: None,
            declared: None, receipt_runner: None,
            classification: crate::validation::classify_validation_script(command),
        };
        let mut signal = json!({});
        attach_command_validation(&mut signal, output.as_bytes(), Some(&validation), Some(0), true);
        assert_eq!(signal["validation_summary_tests"].as_u64(), expected, "{command}: {output}");
        assert!(signal.get("runner_execution_receipt").is_none());
        let mut failed = json!({});
        attach_command_validation(&mut failed, output.as_bytes(), Some(&validation), Some(1), true);
        assert!(failed.get("validation_summary_tests").is_none());
    }
}

#[test]
fn runner_receipts_preserve_execution_tuples_and_partial_failures() {
    let pure = json!({"binary": "core", "helpers": [], "test": "same"});
    let helper = json!({"binary": "core", "helpers": ["helper"], "test": "same"});
    let mut receipt = json!({
        "kind": "codex_test_execution_v1", "runner": "rust_test_runner",
        "runner_input_fingerprint": "a".repeat(64), "selected_targets": ["a", "b"],
        "completed_tests": {"core": ["same"]}, "executed_tests": 2,
        "executions": [pure, helper], "required_executions": [pure, helper],
        "satisfied_gates": {"a": ["same"], "b": ["same"]}, "exit_code": 0,
    });
    assert_eq!(runner_execution_receipt(receipt.to_string().as_bytes(), Some("rust_test_runner")), Some(receipt.clone()));
    receipt["exit_code"] = json!(2);
    receipt["executions"] = json!([pure]);
    receipt["executed_tests"] = json!(1);
    let validation = crate::validation::CommandValidation {
        execution_context: None, declared: None,
        classification: crate::validation::classify_validation_script("cargo test"),
        receipt_runner: Some("rust_test_runner".into()),
    };
    let mut signal = json!({});
    attach_command_validation(&mut signal, receipt.to_string().as_bytes(), Some(&validation), Some(2), true);
    assert_eq!(signal["runner_execution_receipt"], receipt);
    assert!(signal.get("failure_signature").is_some());
    for exit_code in [0, 1] {
        let mut signal = json!({});
        attach_command_validation(&mut signal, receipt.to_string().as_bytes(), Some(&validation), Some(exit_code), true);
        assert!(signal.get("runner_execution_receipt").is_none(), "actual exit must match receipt");
    }
    receipt["exit_code"] = json!(0);
    assert!(runner_execution_receipt(receipt.to_string().as_bytes(), Some("rust_test_runner")).is_none(), "partial selection is not success");
    receipt["exit_code"] = json!(2);
    receipt["executions"] = json!([pure, pure]);
    receipt["executed_tests"] = json!(2);
    assert!(runner_execution_receipt(receipt.to_string().as_bytes(), Some("rust_test_runner")).is_none(), "duplicate execution identities are not two passes");
}

#[tokio::test]
async fn exec_validation_compacts_receipts_without_changing_exact_streams_or_proof() {
    let tests = (0..160).map(|index| format!("module::日本語::test_{index:04}")).collect::<Vec<_>>();
    let receipt = json!({
        "kind": "codex_test_execution_v1", "runner": "rust_test_runner",
        "runner_input_fingerprint": "a".repeat(64), "selected_targets": ["core_lib"],
        "completed_tests": {"codex-core": tests}, "executed_tests": 160,
        "skipped_tests": null, "exit_code": 0,
    });
    let raw = format!("warning: retain this diagnostic\n{receipt}\nRust phase build/test: wall=1.234s; exit=0\n");
    let (mut output, artifact_id, artifact_path, _root) =
        artifact_backed_exec_output(raw.as_bytes(), Some(10_000)).await;
    output.hook_command = Some("cargo test --lib".into());
    output.validation = Some(crate::validation::CommandValidation {
        execution_context: None,
        declared: None,
        classification: crate::validation::classify_validation_script("cargo test --lib"),
        receipt_runner: Some("rust_test_runner".into()),
    });
    output.process_output = Some(std::sync::Arc::new(crate::unified_exec::ProcessOutputSnapshot {
        aggregated_output: raw.as_bytes().to_vec(), stdout: raw.as_bytes().to_vec(), stderr: Vec::new(),
        aggregated_output_is_exact: true, streams_are_exact: true,
    }));
    assert!(codex_utils_string::approx_token_count(&raw) < 10_000);
    let result = output.code_mode_result(&ToolPayload::Function { arguments: "{}".into() });
    let display = result["output"].as_str().unwrap();
    assert!(display.len() * 5 < raw.len(), "{display}");
    assert!(display.contains("codex_test_execution_summary_v1"));
    assert!(display.contains("\"executed_tests\":160"));
    assert!(display.contains("warning: retain this diagnostic"));
    assert!(display.contains("wall=1.234s; exit=0"));
    assert_eq!(result["stdout"], raw);
    assert_eq!(result["streams_complete"], true);
    assert_eq!(result["output_reduced"], true);
    assert_eq!(result["raw_output_artifact_id"], artifact_id.to_string());
    assert_eq!(std::fs::read(artifact_path).unwrap(), raw.as_bytes());
    assert!(output.response_text().contains("codex_test_execution_summary_v1"));
    let mut signal = json!({});
    attach_command_validation(&mut signal, &output.raw_output, output.validation.as_ref(), Some(0), true);
    assert_eq!(signal["runner_execution_receipt"], receipt);
    assert!(runner_execution_receipt(display.as_bytes(), Some("rust_test_runner")).is_none());

    output.validation.as_mut().unwrap().receipt_runner = None;
    assert!(output.summarized_output(&raw, 10_000).is_none(), "untrusted output is not a receipt");
    output.validation.as_mut().unwrap().receipt_runner = Some("rust_test_runner".into());
    output.process_id = Some(7);
    assert!(output.summarized_output(&raw, 10_000).is_none(), "live output must not be summarized as success");
    output.process_id = None;
    output.exit_code = Some(1);
    assert!(output.summarized_output(&raw, 10_000).is_none(), "failures retain existing diagnostics");
    output.exit_code = Some(0);
    output.raw_output_artifact = None;
    assert!(output.summarized_output(&raw, 10_000).is_none(), "never discard unrecoverable evidence");
}

#[tokio::test]
async fn exec_passing_validation_is_compact_even_when_it_fits_the_output_budget() {
    let raw = format!("{}test result: ok. 160 passed; 0 failed; 2 ignored\n",
        (0..160).map(|index| format!("test case_{index:04} ... ok\n")).collect::<String>());
    let (mut output, _, _, _root) = artifact_backed_exec_output(raw.as_bytes(), Some(10_000)).await;
    output.hook_command = Some("cargo test --lib".into());
    output.validation = Some(crate::validation::CommandValidation {
        execution_context: None,
        declared: None,
        classification: crate::validation::classify_validation_script("cargo test --lib"),
        receipt_runner: None,
    });
    let summary = output.summarized_output(&raw, 10_000).expect("compact passing validation");
    assert!(summary.len() < raw.len());
    assert!(summary.contains("160 passed; 0 failed; 2 ignored"));
    output.validation = None;
    assert!(output.summarized_output(&raw, 10_000).is_none(), "ordinary output stays exact below budget");
}

#[tokio::test]
async fn exec_model_output_exposes_artifact_id_not_path() {
    let (output, artifact_id, artifact_path, _retained_root) =
        artifact_backed_exec_output(b"complete output requiring reduction\n", Some(2)).await;

    let response = output.response_text();

    assert!(response.contains(&artifact_id.to_string()));
    assert!(!response.contains(&artifact_path.to_string_lossy().to_string()));
}

#[tokio::test]
async fn exec_code_mode_exposes_artifact_id_not_path() {
    let (mut output, artifact_id, artifact_path, _retained_root) =
        artifact_backed_exec_output(b"complete output\n", Some(1_000)).await;

    let result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });

    assert_eq!(result["raw_output_artifact_id"], artifact_id.to_string());
    assert!(result.get("raw_output_artifact").is_none());
    assert!(
        !result
            .to_string()
            .contains(&artifact_path.to_string_lossy().to_string())
    );

    output.raw_output_artifact = Some(RawOutputArtifact::Failed {
        id: Some(artifact_id),
        message: format!("failed to flush `{}`", artifact_path.display()),
        owned_path: Some(artifact_path.clone()),
        bytes: 7,
    });
    let failed_result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    assert_eq!(
        failed_result["raw_output_artifact_error"],
        "raw output artifact storage failed"
    );
    assert!(
        !failed_result
            .to_string()
            .contains(&artifact_path.to_string_lossy().to_string())
    );
}

#[tokio::test]
async fn exec_code_mode_preserves_empty_output_and_explicit_lifecycle() {
    let (mut output, _, _, _retained_root) = artifact_backed_exec_output(b"", Some(1_000)).await;
    output.raw_output_artifact = None;

    let result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });

    assert_eq!(result["output"], "");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["execution_state"], "exited");
    assert_eq!(result["output_complete"], true);
    output.exit_code = None;
    output.process_exited = false;
    output.process_id = Some(1);
    let running = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".into(),
    });
    assert_eq!(running["output"], "");
    assert_eq!(running["execution_state"], "running");
    assert_eq!(running["output_complete"], false);
}

#[tokio::test]
async fn silence_observation_survives_zero_display_budget() {
    let (mut output, _, _, _root) = artifact_backed_exec_output(b"notice", Some(0)).await;
    output.process_exited = false;
    output.exit_code = None;
    output.process_id = Some(1);
    output.session_capabilities = Some(ExecSessionCapabilities {
        incarnation: uuid::Uuid::nil(), stdin: false, interrupt: false, cancellation: true, polling: true,
        observation: Some(ExecSilenceObservation {
            silent_for_ms: 100,
            reason: ExecObservationReason::NoOutputObserved,
            process_exited: false,
            termination_requested: false,
        }),
    });
    let result = output.code_mode_result_with_budget(&ToolPayload::Function { arguments: "{}".into() }, 0);
    assert_eq!(result["session_capabilities"]["observation"], serde_json::json!({
        "silent_for_ms": 100, "reason": "no_output_observed",
        "process_exited": false, "termination_requested": false,
    }));
}

#[tokio::test]
async fn fork91_empty_terminal_drain_preserves_chunk_and_cumulative_stream_contracts() {
    let raw = b"validation completed: 5 passed\r\n";
    let (mut output, _, _, _root) = artifact_backed_exec_output(raw, Some(1000)).await;
    output.raw_output.clear();
    let payload = ToolPayload::Function { arguments: "{}".into() };
    for (exact, live, expected) in [(true, false, true), (false, false, false), (true, true, false)] {
        output.process_output = Some(std::sync::Arc::new(crate::unified_exec::ProcessOutputSnapshot {
            aggregated_output: raw.to_vec(), stdout: raw.to_vec(), stderr: Vec::new(),
            aggregated_output_is_exact: exact, streams_are_exact: exact,
        }));
        output.process_id = live.then_some(7);
        output.process_exited = !live;
        output.exit_code = (!live).then_some(0);
        let result = output.code_mode_result(&payload);
        assert_eq!(result["output"], "", "terminal polls must not replay earlier chunks");
        assert_eq!(format!("{}{}", String::from_utf8_lossy(raw), result["output"].as_str().unwrap()), String::from_utf8_lossy(raw));
        assert_eq!(result["streams_complete"], expected);
        assert_eq!(result["stdout"].as_str(), expected.then_some("validation completed: 5 passed\r\n"));
        assert_eq!(result["output_complete"], false, "the chunk alone is not cumulative coverage");
        assert!(output.raw_output.is_empty(), "canonical chunk is unchanged");
    }
}

#[tokio::test]
async fn fork91_cumulative_recovery_without_coordinates_does_not_guess_a_prefix() {
    let raw = "retained progress\n".repeat(1000);
    let (mut output, id, _, _root) = artifact_backed_exec_output(raw.as_bytes(), Some(1000)).await;
    output.raw_output = b"last chunk\n".to_vec();
    output.output_ranges = None;
    let payload = ToolPayload::Function { arguments: "{}".into() };
    let result = output.code_mode_result(&payload);
    assert_eq!(result["raw_output_artifact_id"], id.to_string());
    assert!(result.get("recovery_selector").is_none());
    assert!(result.get("recovery").is_none());
    output.raw_output_artifact = None;
    assert!(output.code_mode_result(&payload).get("recovery_selector").is_none());
    output.raw_output = raw.into_bytes();
    output.max_output_tokens = Some(10_000);
    assert!(output.code_mode_result(&payload).get("recovery_selector").is_none());
}

#[tokio::test]
async fn fork91_no_match_requires_exact_error_free_terminal_streams() {
    let (mut output, _, _, _root) = artifact_backed_exec_output(b"", Some(1000)).await;
    output.exit_code = Some(1);
    output.search_no_match = true;
    let payload = ToolPayload::Function { arguments: "{}".into() };
    for (stderr, exact, expected) in [("", true, true), ("rg: missing path (os error 123)", true, false), ("", false, false)] {
        output.process_output = Some(std::sync::Arc::new(crate::unified_exec::ProcessOutputSnapshot {
            aggregated_output: stderr.as_bytes().to_vec(), stdout: Vec::new(), stderr: stderr.as_bytes().to_vec(),
            aggregated_output_is_exact: exact, streams_are_exact: exact,
        }));
        assert_eq!(output.success_for_logging(), expected);
        assert_eq!(output.code_mode_result(&payload)["search_no_match"].as_bool().unwrap_or(false), expected);
    }
    output.process_output = None;
    assert!(!output.success_for_logging(), "unknown streams are not a no-match proof");
    output.exit_code = Some(0);
    assert!(output.success_for_logging(), "ordinary successes are unchanged");
}

#[tokio::test]
async fn nested_command_output_uses_cell_budget_and_preserves_explicit_caps() {
    let raw = "source line with useful evidence\n".repeat(1500);
    let (output, _, _, _root) = artifact_backed_exec_output(raw.as_bytes(), Some(8_000)).await;
    let payload = |cap: Option<usize>| ToolPayload::Function {
        arguments: json!({"max_output_tokens": cap}).to_string(),
    };
    let default = output.code_mode_result_with_budget(&payload(None), 8_000);
    assert_eq!(default["output_reduced"], true);
    for budget in [20_800, 24_000, 32_000] {
        let raised = output.code_mode_result_with_budget(&payload(None), budget);
        assert_eq!(raised["output"], raw);
        assert_eq!(raised["output_complete"], true);
    }
    for cap in [0, 32, 1000] {
        let small = output.code_mode_result_with_budget(&payload(Some(cap)), 24_000);
        assert!(codex_utils_string::approx_token_count(small["output"].as_str().unwrap()) <= cap);
        assert_eq!(small["output_reduced"], true);
        assert_eq!(small["exit_code"], 0);
        assert!(small["raw_output_artifact_id"].is_string());
    }
    let depleted = output.code_mode_result_with_budget(&payload(None), 0);
    assert_eq!(depleted["output"], "");
    assert_eq!(depleted["output_reduced"], true);
}

#[tokio::test]
async fn code_mode_command_result_fits_its_cell_when_printed() {
    // Scripts requested 12000-18000 nested tokens and printed the result. The
    // 10000-token cell then cut the escaped JSON again, without a locator,
    // even where the command had reported its output complete.
    let raw_output = [
        "    pub(crate) fn enter(jobs: usize) -> Self {",
        "        assert!(jobs > 0);",
        "        let previous = JOBS.replace(jobs);",
        "        Self { previous, _thread: PhantomData }",
        "    }",
        "",
        "    /// Restores the previous job count when the scope ends.",
        "    fn drop(&mut self) {",
        "        JOBS.set(self.previous);",
        "    }",
    ]
    .map(|line| format!("{line}\r\n"))
    .concat()
    .repeat(400);
    let (mut output, artifact_id, _, _retained_root) =
        artifact_backed_exec_output(raw_output.as_bytes(), Some(18_000)).await;
    output.hook_command = Some("Get-Content src/analysis_jobs.rs".to_string());

    let result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    // `text(result)` prints this serialization; the cell cap counts model tokens.
    let printed = result.to_string();

    assert!(
        codex_utils_output_truncation::model_token_count(&printed)
            <= codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL,
        "a printed nested result must fit its cell"
    );
    assert_eq!(result["output_reduced"], true);
    assert_eq!(result["output_complete"], false);
    assert_eq!(result["raw_output_artifact_id"], artifact_id.to_string());
    // Each gap names its raw-output lines, so a reader recovers exactly the
    // missing lines instead of rereading the file. Periodic braces and blank
    // lines cannot shift a gap or the selector derived from it.
    let source = raw_output.lines().collect::<Vec<_>>();
    let mut rest = result["output"]
        .as_str()
        .unwrap()
        .split_once("\n\n")
        .expect("truncation header")
        .1;
    let (mut next, mut gaps) = (1, Vec::new());
    while let Some((retained, marked)) = rest.split_once("\n[omitted lines ") {
        let (marker, after) = marked.split_once("]\n").expect("marker end");
        let (span, total) = marker.split_once(" of ").expect("marker total");
        assert_eq!(total, source.len().to_string(), "whole boundary lines");
        let (first, last) = span.split_once('-').expect("marker span");
        let (first, last) = (first.parse::<usize>().unwrap(), last.parse::<usize>().unwrap());
        assert_eq!(retained.lines().collect::<Vec<_>>(), source[next - 1..first - 1].to_vec());
        gaps.push((first, last));
        (next, rest) = (last + 1, after);
    }
    assert_eq!(rest.lines().collect::<Vec<_>>(), source[next - 1..].to_vec());
    assert_eq!(gaps.len(), 2);
    let (first, last) = gaps[0];
    assert_eq!(
        result["recovery_selector"],
        json!({"kind": "lines", "start": first, "end": last.min(first + 199)})
    );
    let direct: JsonValue = serde_json::from_str(&output.response_text()).unwrap();
    assert!(
        codex_utils_string::approx_token_count(direct["output"].as_str().unwrap())
            <= 10_000,
        "the direct response must cap oversized requests"
    );
    assert_eq!(output.model_output_limits(&raw_output, None).applied_limit, 10_000);
    assert_eq!(direct["artifact_id"], artifact_id.to_string());
}

#[tokio::test]
async fn token_efficiency_artifact_recovery_notice_does_not_repeat_id() {
    let raw_output = "word ".repeat(200);
    // Leave room for the complete locator and selector while still reducing
    // the producer text. Smaller budgets may retain only the artifact header.
    let (output, artifact_id, _, _retained_root) =
        artifact_backed_exec_output(raw_output.as_bytes(), Some(200)).await;

    let response = output.response_text();

    let packet: JsonValue = serde_json::from_str(&response).unwrap();
    assert_eq!(packet["artifact_id"], artifact_id.to_string());
    assert_eq!(response.matches(&artifact_id.to_string()).count(), 2);
    assert_eq!(packet["recovery"]["arguments"]["artifact_id"], artifact_id.to_string());
    assert!(codex_utils_string::approx_token_count(packet["output"].as_str().unwrap()) <= 200);
    assert!(packet["recovery"]["arguments"]["selectors"].is_array());
    let code_mode = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".into(),
    });
    assert_eq!(code_mode["raw_output_artifact_id"], artifact_id.to_string());
    assert!(
        !code_mode["output"]
            .as_str()
            .unwrap()
            .contains(&artifact_id.to_string())
    );
}

#[tokio::test]
async fn exec_reduction_notice_is_absent_for_complete_output() {
    let (output, _, _, _retained_root) =
        artifact_backed_exec_output(b"complete output\n", Some(1_000)).await;

    let response = output.response_text();

    assert!(!response.contains("[command output reduced;"));
}

#[tokio::test]
async fn exec_reduction_notice_is_absent_after_artifact_is_evicted() {
    let raw_output = "word ".repeat(200);
    let (output, _, artifact_path, _retained_root) =
        artifact_backed_exec_output(raw_output.as_bytes(), Some(4)).await;
    std::fs::remove_file(&artifact_path).expect("evict retained artifact");

    let response = output.response_text();

    assert!(!response.contains("[command output reduced;"));
    assert!(!response.contains("full retained output is available"));

    std::fs::create_dir(&artifact_path).expect("replace artifact with nonregular entry");
    let nonregular_response = output.response_text();
    assert!(!nonregular_response.contains("[command output reduced;"));
    assert!(!nonregular_response.contains("full retained output is available"));
}

#[tokio::test]
async fn audit_tiny_recovery_budget_preserves_process_state() {
    let (mut output, _, _, _root) =
        artifact_backed_exec_output("large output ".repeat(1000).as_bytes(), Some(4)).await;
    output.process_id = Some(42);
    output.process_exited = false;
    output.exit_code = None;
    assert!(output.response_text().contains("\"session_id\":42"));
    output.process_exited = true;
    output.exit_code = Some(7);
    assert!(output.response_text().contains("\"exit_code\":7"));
    output.exit_code = None;
    assert!(
        output
            .response_text()
            .contains("Process exited without an available exit code")
    );
}
#[tokio::test]
async fn exec_projection_retains_diagnostics_outside_generic_cut_regions() {
    let mut raw = "ordinary build progress\n".repeat(2000);
    raw.push_str("error[E0123]: unique important failure\n");
    raw.push_str(&"ordinary build progress\n".repeat(8000));
    let (mut output, _, _, _root) = artifact_backed_exec_output(raw.as_bytes(), Some(1000)).await;
    output.exit_code = Some(1);
    output.hook_command = Some("cargo check".into());
    let metadata = output.projection_metadata_from_raw(&raw);
    assert!(metadata.fragments.iter().any(|fragment|
        fragment.kind == ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary
        && fragment.text.contains("unique important failure")));
    assert!(metadata.predetermined_ranges.is_empty());
    let ranges = predetermined_validation_ranges(&raw, Some("cargo check"));
    assert_eq!(ranges[0].start_line, 2001);
}
