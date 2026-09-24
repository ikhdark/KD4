use super::*;

fn options(
    command_text: Option<&str>,
    applied_token_limit: Option<usize>,
) -> ShellOutputSummaryOptions<'_> {
    ShellOutputSummaryOptions {
        enabled: true,
        applied_token_limit,
        command_text,
    }
}

#[test]
fn go_and_pytest_failures_survive_passing_output_afterward() {
    for (command, failure) in [
        ("go test ./...", "--- FAIL: TestExpectedResult (0.00s)"),
        (
            "pytest",
            "FAILED tests/test_example.py::test_expected - AssertionError",
        ),
    ] {
        let output = format!(
            "{}\n{failure}\nexpected 2, got 1\n{}",
            "build progress\n".repeat(100),
            "unrelated passing test\n".repeat(700)
        );
        let summary =
            summarize_shell_output_for_model(&output, 1, false, options(Some(command), None))
                .unwrap();
        assert!(summary.contains(failure), "{summary}");
        assert!(summary.contains("expected 2, got 1"), "{summary}");
    }
}

#[test]
fn a_successful_flat_list_is_truncated_rather_than_summarized() {
    // `git status --short` over a large working tree: hundreds of uniform
    // lines, no diagnostics. Head/tail selection would drop the middle for no
    // reason, and the result would read as a summary rather than a truncation.
    let output = (0..600)
        .map(|index| format!(" M codex-rs/crate{index:04}/src/lib.rs"))
        .collect::<Vec<_>>()
        .join("\n");

    let summary = summarize_shell_output_for_model(
        &output,
        0,
        false,
        options(Some("git status --short"), None),
    );

    assert_eq!(
        summary, None,
        "a successful flat list must fall through to ordinary truncation"
    );
}

#[test]
fn a_successful_run_with_diagnostics_is_still_summarized() {
    // The same shape, but one warning makes ranking meaningful again. Without
    // this the guard above would disable summarization for every exit-zero run.
    let mut lines = (0..600)
        .map(|index| format!("compiling crate{index:04}"))
        .collect::<Vec<_>>();
    lines[300] = "warning: unused variable `handle`".into();
    let output = lines.join("\n");

    let summary =
        summarize_shell_output_for_model(&output, 0, false, options(Some("cargo build"), None))
            .expect("diagnostic output must still be summarized");

    assert!(
        summary.contains("warning: unused variable `handle`"),
        "{summary}"
    );
}

#[test]
fn a_failing_flat_list_is_still_summarized() {
    // A non-zero exit is itself the signal; the tail matters even when no line
    // matches a diagnostic pattern.
    let output = (0..600)
        .map(|index| format!("processed item {index:04}"))
        .collect::<Vec<_>>()
        .join("\n");

    let summary =
        summarize_shell_output_for_model(&output, 1, false, options(Some("custom-command"), None));

    assert!(
        summary.is_some(),
        "a failing command must keep its focused summary"
    );
}

#[test]
fn small_output_is_unchanged() {
    let output = "ok\n";

    let summary = summarize_shell_output_for_model(output, 0, false, options(None, None));

    assert_eq!(summary, None);
}

#[test]
fn source_reads_use_ordered_truncation_in_the_normal_output_path() {
    let mut lines = (0..700)
        .map(|index| format!("source line {index:04}: {}", "x".repeat(80)))
        .collect::<Vec<_>>();
    lines[350] = "error[E9999]: this is source text, not a compiler diagnostic".into();
    let output = lines.join("\n");
    let exec_output = codex_protocol::exec_output::ExecToolCallOutput {
        exit_code: 0,
        stdout: codex_protocol::exec_output::StreamOutput::new(output.clone()),
        stderr: codex_protocol::exec_output::StreamOutput::new(String::new()),
        aggregated_output: codex_protocol::exec_output::StreamOutput::new(output),
        duration: std::time::Duration::ZERO,
        timed_out: false,
    };
    for command in ["sed -n '1,700p' src/lib.rs", "rg -n pattern src/lib.rs"] {
        let projected = crate::tools::project_exec_output_for_model_with_budget(
            &exec_output,
            codex_utils_output_truncation::TruncationPolicy::Tokens(2_000),
            Some(2_000),
            Some(command),
        );
        assert!(projected.reduced, "{command}");
        assert!(projected.text.contains("source line 0000"), "{command}");
        assert!(
            !projected.text.contains("Shell output summary:"),
            "{command}"
        );
        // Ordered truncation keeps a head/middle/tail window in source order.
        // The summarizer would instead rank this source text as a diagnostic
        // and hoist it above the surrounding lines.
        assert!(
            projected.text.contains("[omitted before retained middle]"),
            "{command}"
        );
        if let Some(diagnostic) = projected.text.find("error[E9999]") {
            let head = projected
                .text
                .find("source line 0000")
                .expect("retained head line");
            assert!(head < diagnostic, "{command}");
        }
    }
}

#[test]
fn validation_output_uses_structured_wrapper_classification() {
    let output = (0..700)
        .map(|index| format!("ordinary output {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let direct =
        summarize_shell_output_for_model(&output, 0, false, options(Some("cargo test"), None))
            .unwrap();
    for command in [
        "cargo +stable --offline test",
        "env MODE=test cargo test",
        "just --justfile tasks.just core-test-fast core_lib focused",
    ] {
        assert_eq!(
            summarize_shell_output_for_model(&output, 0, false, options(Some(command), None)),
            Some(direct.clone()),
            "{command}"
        );
    }
    let prose = summarize_shell_output_for_model(
        &output,
        0,
        false,
        options(Some("printf 'cargo test'"), None),
    );
    assert_eq!(
        prose, None,
        "a string argument is not a validation invocation"
    );
}

#[test]
fn oversized_diagnostic_and_distinct_middle_failure_preserve_final_status() {
    let mut lines = vec!["ordinary output".to_string(); 900];
    lines[20] = format!("error: EARLY_DIAGNOSTIC {}", "x".repeat(50_000));
    for index in (40..700).step_by(10) {
        lines[index] = "error[E0001]: repeated failure".into();
    }
    lines[350] = "error[E0002]: UNIQUE_MIDDLE_FAILURE".into();
    lines[351] = "  --> src/middle.rs:7:3".into();
    lines[899] = "test result: FAILED. 12 passed; 2 failed".into();
    let output = lines.join("\n");
    let exec_output = codex_protocol::exec_output::ExecToolCallOutput {
        exit_code: 1,
        stdout: codex_protocol::exec_output::StreamOutput::new(output.clone()),
        stderr: codex_protocol::exec_output::StreamOutput::new(String::new()),
        aggregated_output: codex_protocol::exec_output::StreamOutput::new(output),
        duration: std::time::Duration::ZERO,
        timed_out: false,
    };
    let summary = crate::tools::project_exec_output_for_model_with_budget(
        &exec_output,
        codex_utils_output_truncation::TruncationPolicy::Tokens(10_000),
        Some(10_000),
        None,
    )
    .text;
    for expected in [
        "EARLY_DIAGNOSTIC",
        "UNIQUE_MIDDLE_FAILURE",
        "src/middle.rs:7:3",
        "test result: FAILED. 12 passed; 2 failed",
        "[line truncated]",
    ] {
        assert!(summary.contains(expected), "missing {expected}: {summary}");
    }
    assert!(summary.find("EARLY_DIAGNOSTIC") < summary.find("UNIQUE_MIDDLE_FAILURE"));
    assert!(summary.find("UNIQUE_MIDDLE_FAILURE") < summary.find("test result: FAILED"));
}

#[test]
fn unretained_distinct_diagnostics_are_counted() {
    let mut lines = vec!["ordinary output".to_string(); 900];
    for index in 0..12 {
        lines[40 + index * 20] = format!("fatal: distinct category {index}");
    }
    let summary =
        summarize_shell_output_for_model(&lines.join("\n"), 1, false, options(None, None)).unwrap();
    assert!(
        summary.contains("omitted_diagnostic_groups: 4; inspect the raw output"),
        "{summary}"
    );
    assert!(summary.contains("fatal: distinct category 0"));
    assert!(summary.contains("fatal: distinct category 11"));
}

#[test]
fn ordinary_try_prose_does_not_displace_diagnostics() {
    let mut lines = (0..900)
        .map(|index| format!("ordinary line {index}"))
        .collect::<Vec<_>>();
    for index in 0..20 {
        lines[50 + index * 10] = "try another example".to_string();
    }
    lines[400] = "warning: KEEP_REAL_DIAGNOSTIC".to_string();
    let summary = summarize_shell_output_for_model(
        &lines.join("\n"),
        0,
        false,
        options(Some("cargo test"), None),
    )
    .expect("validation summary");
    assert!(summary.contains("KEEP_REAL_DIAGNOSTIC"), "{summary}");
    assert!(!summary.contains("try another example"), "{summary}");
}

/// An oversized first line that is also a diagnostic, so the successful output
/// still has something to rank and reaches the summarizer's line cap logic.
fn oversized_warning_line(total_bytes: usize) -> String {
    const LABEL: &str = "warning: ";
    format!("{LABEL}{}", "x".repeat(total_bytes - LABEL.len()))
}

#[test]
fn oversized_first_line_reserves_room_for_the_tail() {
    let mut lines = vec![String::new(); 700];
    lines[0] = oversized_warning_line(SUMMARY_MAX_BYTES);
    // Exercise the byte ceiling independently of a tighter caller token limit.
    let probe =
        summarize_shell_output_for_model(&lines.join("\n"), 0, false, options(None, None))
            .unwrap();
    let prefix_bytes = probe.find("    1: ").unwrap() + "    1: ".len();
    lines[0] = oversized_warning_line(SUMMARY_MAX_BYTES - SUMMARY_FOOTER_BYTES - prefix_bytes);
    let summary =
        summarize_shell_output_for_model(&lines.join("\n"), 0, false, options(None, None))
            .unwrap();
    assert!(summary.ends_with("[summary capped]"), "{summary}");
    assert!(summary.contains("- emitted_source_lines: 88\n"));
    // A final empty line is not a source line according to str::lines().
    assert!(summary.contains("- omitted_source_lines: 611\n"));
    assert!(summary.contains("[line truncated]"));
    assert!(summary.contains("  699: "));
    assert!(summary.len() <= SUMMARY_MAX_BYTES);
}

#[test]
fn summary_reports_gap_sizes_and_source_line_counts() {
    let mut lines = (0..700)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>();
    // A diagnostic inside the retained head keeps the selection unchanged
    // while giving a successful run something to rank.
    lines[0] = "warning: line 0".to_string();
    let output = lines.join("\n");
    let summary =
        summarize_shell_output_for_model(&output, 0, false, options(None, Some(1_000))).unwrap();
    assert!(summary.contains("... [612 lines omitted]"));
    assert!(summary.contains("- emitted_source_lines: 88\n"));
    assert!(summary.ends_with("- omitted_source_lines: 612"));
    assert!(!summary.contains("[summary capped]"));
}

#[test]
fn summary_does_not_end_with_a_gap_when_the_following_line_cannot_fit() {
    let mut lines = vec!["ordinary".to_string(); 700];
    lines[0] = oversized_warning_line(SUMMARY_MAX_BYTES);
    // Exercise the byte ceiling independently of a tighter caller token limit.
    let probe =
        summarize_shell_output_for_model(&lines.join("\n"), 0, false, options(None, None))
            .expect("large output summary");
    let prefix_bytes = probe.find("    1: ").expect("first source line") + "    1: ".len();
    let following_head_bytes = (SUCCESS_HEAD_LINES - 1) * "\n    2: ordinary".len();
    let gap_bytes = "\n... [612 lines omitted]".len();
    lines[0] = oversized_warning_line(
        SUMMARY_MAX_BYTES - SUMMARY_FOOTER_BYTES - prefix_bytes - following_head_bytes - gap_bytes,
    );

    let summary =
        summarize_shell_output_for_model(&lines.join("\n"), 0, false, options(None, None))
            .expect("large output summary");
    let (body, _) = summary
        .split_once("\n- emitted_source_lines:")
        .expect("retention counts");
    assert!(body.ends_with("  700: ordinary"));
    assert!(body.contains("... [612 lines omitted]\n  637: ordinary"));
    assert!(summary.contains("- emitted_source_lines: 88\n"));
    assert!(summary.contains("- omitted_source_lines: 612\n"));
    assert!(summary.ends_with("[summary capped]"));
    assert!(summary.len() <= SUMMARY_MAX_BYTES);
}

#[test]
fn source_prose_about_passed_does_not_displace_test_status() {
    let mut lines = vec!["ordinary output".to_string(); 700];
    lines[200] = "test result: ok. 3 passed; 0 failed".to_string();
    for line in &mut lines[300..320] {
        *line = "let message = \"passed\"; // source text".to_string();
    }
    let summary = summarize_shell_output_for_model(
        &lines.join("\n"),
        0,
        false,
        options(Some("cargo test"), None),
    )
    .unwrap();
    assert!(summary.contains("test result: ok. 3 passed; 0 failed"));
    assert!(!summary.contains("// source text"));
}

#[test]
fn incidental_words_do_not_displace_validation_status_or_warnings() {
    let mut lines = (0..700)
        .map(|index| format!("ordinary line {index}"))
        .collect::<Vec<_>>();
    for index in 0..12 {
        lines[50 + index * 8] = format!("warnings: 0 (batch {index})");
        lines[300 + index * 8] =
            format!("bypassed, surpassed, passed-through, not_passed, forewarning (batch {index})");
    }
    lines[200] = "warning[E001]: KEEP_ADVISORY".to_string();
    lines[250] = "Tests PASSED; KEEP_STATUS".to_string();
    let summary = summarize_shell_output_for_model(
        &lines.join("\n"),
        0,
        false,
        options(Some("cargo test"), None),
    )
    .expect("large validation summary");

    assert!(summary.contains("KEEP_ADVISORY"), "{summary}");
    assert!(summary.contains("KEEP_STATUS"), "{summary}");
    assert!(!summary.contains("warnings: 0"), "{summary}");
    assert!(!summary.contains("passed-through"), "{summary}");
}

#[test]
fn selected_errors_do_not_spend_the_final_status_quota() {
    let mut lines = (0..900)
        .map(|index| format!("ordinary line {index}"))
        .collect::<Vec<_>>();
    for index in 0..8 {
        lines[50 + index * 10] = format!("suite {index} passed; KEEP_STATUS_{index}");
        lines[300 + index * 10] = format!("compiler error: KEEP_ERROR_{index}");
    }
    for line in &mut lines[500..520] {
        *line = "let summary: String = source_text;".to_string();
    }
    // Exercise the normal exec-output projection boundary, not only selection helpers.
    let output = codex_protocol::exec_output::ExecToolCallOutput {
        exit_code: 1,
        stdout: codex_protocol::exec_output::StreamOutput::new(lines.join("\n")),
        stderr: codex_protocol::exec_output::StreamOutput::new(String::new()),
        aggregated_output: codex_protocol::exec_output::StreamOutput::new(lines.join("\n")),
        duration: std::time::Duration::ZERO,
        timed_out: false,
    };
    let summary = crate::tools::project_exec_output_for_model_with_budget(
        &output,
        codex_utils_output_truncation::TruncationPolicy::Tokens(3_000),
        Some(3_000),
        None,
    )
    .text;
    assert!(summary.contains("Shell output summary:"), "{summary}");
    assert!(!summary.contains("let summary:"), "{summary}");

    for index in 0..8 {
        assert!(
            summary.contains(&format!("KEEP_STATUS_{index}")),
            "{summary}"
        );
        assert!(
            summary.contains(&format!("KEEP_ERROR_{index}")),
            "{summary}"
        );
    }
}

#[test]
fn large_success_output_keeps_head_tail_and_warning_lines() {
    let mut lines = Vec::new();
    for index in 0..700 {
        lines.push(format!("line {index}"));
    }
    lines[200] = "warning: useful warning".to_string();
    let output = lines.join("\n");

    let summary = summarize_shell_output_for_model(&output, 0, false, options(None, None)).unwrap();

    assert!(summary.contains("Shell output summary:"));
    assert!(summary.contains("line 0"));
    assert!(summary.contains("useful warning"));
    assert!(summary.contains("line 699"));
    assert!(summary.len() <= SUMMARY_MAX_BYTES + "[summary capped]".len() + 1);
    assert!(summary.lines().count() <= SUMMARY_MAX_LINES + 1);
}

#[test]
fn large_success_output_preserves_source_order() {
    let mut lines = (0..700)
        .map(|index| format!("ordinary line {index}"))
        .collect::<Vec<_>>();
    lines[0] = "UNIQUE_HEAD".to_string();
    lines[200] = "warning: UNIQUE_MIDDLE".to_string();
    lines[699] = "UNIQUE_TAIL".to_string();
    let output = lines.join("\n");

    let summary = summarize_shell_output_for_model(&output, 0, false, options(None, None)).unwrap();
    let head = summary.find("UNIQUE_HEAD").unwrap();
    let middle = summary.find("UNIQUE_MIDDLE").unwrap();
    let tail = summary.find("UNIQUE_TAIL").unwrap();

    assert!(head < middle && middle < tail);
}

#[test]
fn failed_output_keeps_exact_error_lines() {
    let mut lines = Vec::new();
    for index in 0..700 {
        lines.push(format!("line {index}"));
    }
    lines[175] = "error[E0425]: cannot find value `needle` in this scope".to_string();
    lines[176] = "  --> src/main.rs:10:5".to_string();
    lines[177] = "expected `usize`, actual `String`".to_string();
    let output = lines.join("\n");

    let summary = summarize_shell_output_for_model(&output, 1, false, options(None, None)).unwrap();

    assert!(summary.contains("error[E0425]: cannot find value `needle` in this scope"));
    assert!(summary.contains("--> src/main.rs:10:5"));
    assert!(summary.contains("expected `usize`, actual `String`"));
    assert!(summary.contains("line 699"));
}

#[test]
fn critical_error_survives_earlier_warning_flood() {
    let mut lines = (0..900)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>();
    for warning_index in 0..47 {
        let line_index = 20 + warning_index * 8;
        lines[line_index] = format!("warning: noisy advisory {line_index}");
    }
    lines[420] = "error[E0599]: no method named `repair_bug` found".to_string();
    let output = lines.join("\n");

    let summary = summarize_shell_output_for_model(
        &output,
        1,
        false,
        options(Some("cargo test -p codex-core"), None),
    )
    .unwrap();

    assert!(summary.contains("error[E0599]: no method named `repair_bug` found"));
    assert!(summary.contains("line 419"));
    assert!(summary.contains("line 421"));
    assert!(summary.contains("line 899"));
}

#[test]
fn benign_keywords_do_not_hide_the_first_real_error() {
    let mut lines = (0..900)
        .map(|index| format!("ordinary line {index}"))
        .collect::<Vec<_>>();
    for keyword_index in 0..48 {
        let line_index = 20 + keyword_index * 8;
        lines[line_index] = format!("expected benign value; actual benign value {keyword_index}");
    }
    lines[500] = "error: REAL_ERROR_SENTINEL".to_string();
    let output = lines.join("\n");

    let summary = summarize_shell_output_for_model(&output, 1, false, options(None, None)).unwrap();

    assert!(summary.contains("error: REAL_ERROR_SENTINEL"));
    assert!(summary.contains("ordinary line 499"));
    assert!(summary.contains("ordinary line 501"));
}

#[test]
fn over_truncation_failure_focus_keeps_late_root_cause_after_early_error_flood() {
    let mut lines = (0..900)
        .map(|index| format!("ordinary line {index}"))
        .collect::<Vec<_>>();
    for error_index in 0..20 {
        lines[20 + error_index * 8] = format!("error: noisy precursor {error_index}");
    }
    lines[610] = "fatal: ROOT_CAUSE_SENTINEL".to_string();
    let output = lines.join("\n");

    let summary = summarize_shell_output_for_model(&output, 1, false, options(None, None)).unwrap();

    assert!(summary.contains("ROOT_CAUSE_SENTINEL"));
    assert!(summary.contains("ordinary line 609"));
    assert!(summary.contains("ordinary line 611"));
    assert!(summary.contains("ordinary line 899"));
}

#[test]
fn validation_output_keeps_failure_status_and_tail() {
    let mut lines = Vec::new();
    for index in 0..700 {
        lines.push(format!("test log {index}"));
    }
    lines[80] = "thread 'parser::tests::keeps_error' panicked at src/parser.rs:9:5".to_string();
    lines[260] = "failures: parser::tests::keeps_error".to_string();
    lines[300] = "test result: FAILED. 12 passed; 1 failed".to_string();
    let output = lines.join("\n");

    let summary = summarize_shell_output_for_model(
        &output,
        101,
        false,
        options(Some("cargo test -p codex-core"), None),
    )
    .unwrap();

    assert!(summary.contains("thread 'parser::tests::keeps_error' panicked"));
    assert!(summary.contains("failures: parser::tests::keeps_error"));
    assert!(summary.contains("test result: FAILED. 12 passed; 1 failed"));
    assert!(summary.contains("test log 699"));
}

#[test]
fn early_failed_suite_survives_later_passing_suites() {
    let mut lines = (0..900)
        .map(|index| format!("ordinary line {index}"))
        .collect::<Vec<_>>();
    let failure = "test result: FAILED. 12 passed; 1 failed; 0 ignored";
    lines[100] = failure.to_string();
    for index in 0..20 {
        lines[200 + index * 20] = "test result: ok. 10 passed; 0 failed; 0 ignored".to_string();
    }
    let summary = summarize_shell_output_for_model(
        &lines.join("\n"),
        101,
        false,
        options(Some("cargo test --no-fail-fast"), None),
    )
    .expect("validation summary");

    assert!(summary.contains(failure), "{summary}");
    assert!(summary.contains("test result: ok. 10 passed"), "{summary}");
    assert!(summary.contains("ordinary line 899"), "{summary}");
    assert!(summary.lines().count() <= SUMMARY_MAX_LINES);
}

#[test]
fn validation_output_keeps_the_authoritative_final_status() {
    let mut lines = (0..900)
        .map(|index| format!("ordinary line {index}"))
        .collect::<Vec<_>>();
    for status_index in 0..48 {
        let line_index = 20 + status_index * 8;
        lines[line_index] = format!("test case {status_index} passed");
    }
    lines[500] = "FINAL_STATUS_SENTINEL test result: ok".to_string();
    let output = lines.join("\n");

    let summary = summarize_shell_output_for_model(
        &output,
        0,
        false,
        options(Some("cargo test -p codex-core"), None),
    )
    .unwrap();

    assert!(summary.contains("failure-focused lines, final status lines, tail"));
    assert!(summary.contains("FINAL_STATUS_SENTINEL test result: ok"));
}

#[test]
fn nextest_failures_and_summary_survive_a_passing_test_flood() {
    let mut lines = (0..900)
        .map(|index| format!("PASS [0.001s] crate test_{index}"))
        .collect::<Vec<_>>();
    lines[450] = "FAIL [0.003s] codex_core parser::tests::keeps_error".to_string();
    lines[451] = "TRY 2 FAIL [0.003s] codex_core parser::tests::retry_error".to_string();
    lines[500] = "Summary [1.234s] 900 tests run: 898 passed, 2 failed".to_string();
    let output = lines.join("\n");
    let summary = summarize_shell_output_for_model(
        &output,
        100,
        false,
        options(Some("just core-gate parser"), Some(2_000)),
    )
    .unwrap();
    assert!(summary.contains("parser::tests::keeps_error"));
    assert!(summary.contains("parser::tests::retry_error"));
    assert!(summary.contains("900 tests run: 898 passed, 2 failed"));
}

#[test]
fn passing_validation_retains_status_with_a_short_tail() {
    let mut lines = (0..700)
        .map(|index| format!("PASS [0.001s] crate test_{index}"))
        .collect::<Vec<_>>();
    lines[699] = "Summary [1.234s] 699 tests run: 699 passed".to_string();
    let summary = summarize_shell_output_for_model(
        &lines.join("\n"),
        0,
        false,
        options(Some("just core-test-fast core_lib"), None),
    )
    .unwrap();
    assert!(summary.contains("699 tests run: 699 passed"));
    assert!(summary.contains("test_698"));
    assert!(!summary.contains("test_600"));
    let retained_tests = summary
        .lines()
        .filter_map(|line| line.split_once(": ").map(|(_, text)| text))
        .filter(|line| line.starts_with("PASS [0.001s]"))
        .collect::<Vec<_>>();
    let tail_start = lines.len() - VALIDATION_SUCCESS_TAIL_LINES;
    assert_eq!(
        retained_tests,
        lines[tail_start..lines.len() - 1]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "successful validation must retain exactly the configured tail"
    );
}

#[test]
fn applied_budget_summarizes_output_below_the_default_threshold() {
    let mut lines = (0..200)
        .map(|index| format!("guard line {index}"))
        .collect::<Vec<_>>();
    lines[100] = "warning: guard line 100 needs review".to_string();
    let output = lines.join("\n");

    assert_eq!(
        summarize_shell_output_for_model(&output, 0, false, options(None, None)),
        None
    );
    let summary =
        summarize_shell_output_for_model(&output, 0, false, options(None, Some(400))).unwrap();
    assert!(summary.contains("guard line 199"), "{summary}");
    assert!(
        summary.contains("warning: guard line 100 needs review"),
        "{summary}"
    );
}

#[test]
fn diagnostic_free_output_over_the_applied_budget_falls_through_to_truncation() {
    // A successful listing that merely exceeds the caller's budget is source
    // material. Summarizing it drops the middle and reads as complete, which
    // sent the model back to re-read the same file in the recorded sessions.
    let output = (0..200)
        .map(|index| format!("guard line {index}"))
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        summarize_shell_output_for_model(&output, 0, false, options(None, Some(400))),
        None
    );
    assert_eq!(
        summarize_shell_output_for_model(
            &output,
            0,
            false,
            options(Some("custom-listing --all"), Some(400))
        ),
        None
    );
}

#[test]
fn powershell_read_pipelines_use_ordered_truncation() {
    let mut lines = (0..300)
        .map(|index| format!("source line {index:04}: {}", "x".repeat(40)))
        .collect::<Vec<_>>();
    lines[150] = "// error: this comment is source text, not a diagnostic".to_string();
    let output = lines.join("\n");

    for command in [
        "Get-Content src/lib.rs | Select-Object -Skip 10 -First 500; rg -n 'fn ' src",
        "$s = Get-Content src/lib.rs; $s[10..40]; git diff --stat",
        "foreach ($p in @('a','b')) { if (Test-Path $p) { Get-Content -Raw $p } }",
        "Get-ChildItem src -Recurse -File | Where-Object { $_.Name -match 'test' } | Select-Object FullName",
        // Quoted alternations are patterns, not pipelines.
        "Get-Content src/plan.rs -TotalCount 220; rg -n 'fn |clone|parallel|cache' src/jobs.rs",
        "Get-Process | Where-Object { $_.Name -match '^kda-.*\\.exe$|^cargo\\.exe$' } | Format-List",
        "Get-Content src/lib.rs # don't summarize; Remove-Item is only prose here",
        "$r = Get-Content audit.json -Raw | ConvertFrom-Json -Depth 100; $r.summary | ConvertTo-Json -Compress",
        "$r.passes | ForEach-Object { [pscustomobject]@{pass=$_.Name;seconds=[math]::Round($_.micros/1e6,2)} } | Sort-Object seconds | Format-Table -Wrap",
    ] {
        assert_eq!(
            summarize_shell_output_for_model(&output, 0, false, options(Some(command), Some(400))),
            None,
            "{command}"
        );
    }

    // A script that also mutates or builds is not a read; its diagnostics still rank.
    for command in [
        "Get-Content src/lib.rs; Remove-Item src/old.rs",
        "Get-Content src/lib.rs; cargo build",
        "git checkout -- src/lib.rs; Get-Content src/lib.rs",
        "Get-ChildItem src | ForEach-Object { Set-Content $_ '' }",
        "rg -n 'a|b' src; Remove-Item 'old|new.rs'",
        // A single quote inside a double-quoted string must not hide a command.
        "Write-Output \"it's\"; Remove-Item src/old.rs; Write-Output 'done'",
        "Get-Content src/lib.rs; $removed = Remove-Item src/old.rs",
        "Write-Output \"$(Remove-Item src/old.rs)\"",
        "[IO.File]::Delete('src/old.rs'); Get-Content src/lib.rs",
    ] {
        assert!(
            summarize_shell_output_for_model(&output, 0, false, options(Some(command), Some(400)))
                .is_some(),
            "{command}"
        );
    }
}

#[test]
fn dense_output_over_token_budget_keeps_middle_diagnostics() {
    let mut lines = vec!["{}[]():,;".repeat(3); 500];
    lines[250] = "error: unique middle diagnostic".to_string();
    lines[499] = "test result: FAILED".to_string();
    let output = lines.join("\n");
    let limit = 4000;
    assert!(output.len() < codex_utils_string::approx_bytes_for_tokens(limit));
    assert!(codex_utils_string::approx_token_count(&output) > limit);
    let exec_output = codex_protocol::exec_output::ExecToolCallOutput {
        exit_code: 1,
        aggregated_output: codex_protocol::exec_output::StreamOutput::new(output),
        ..Default::default()
    };
    let projected = crate::tools::project_exec_output_for_model_with_budget(
        &exec_output,
        codex_utils_output_truncation::TruncationPolicy::Tokens(limit),
        Some(limit),
        Some("cargo test"),
    );
    assert!(projected.reduced);
    assert!(projected.text.contains("Shell output summary:"));
    assert!(projected.text.contains("error: unique middle diagnostic"));
    assert!(projected.text.contains("test result: FAILED"));
    assert!(codex_utils_string::approx_token_count(&projected.text) <= limit);
}

#[test]
fn output_just_over_budget_keeps_most_lines_after_diagnostic_pruning() {
    let mut lines = (0..115)
        .map(|index| format!("{index:03}: let value_{index} = compute(input_{index}, &mut state);"))
        .collect::<Vec<_>>();
    lines[111] = "warning: in the working copy of 'README.md', CRLF will be replaced by LF".into();
    lines[113] = "SOURCEMAP.md:242: declared owner has no repository source: .github".into();
    let output = lines.join("\n");
    let limit = codex_utils_string::approx_token_count(&output) * 9 / 10;

    let summary = summarize_shell_output_for_model(
        &output,
        1,
        false,
        options(Some("python scripts/source_map_check.py"), Some(limit)),
    )
    .expect("failed output over its budget is summarized");

    assert!(!codex_utils_string::approx_token_count_exceeds(
        &summary, limit
    ));
    let emitted = summary
        .split_once("- emitted_source_lines: ")
        .and_then(|(_, rest)| rest.lines().next())
        .and_then(|count| count.parse::<usize>().ok())
        .expect("emitted line count");
    assert!(emitted >= lines.len() / 2, "{summary}");
    // Budget returns to the tail first; the diagnostic context stays intact.
    assert!(summary.contains("let value_100 ="), "{summary}");
    assert!(
        summary.contains("warning: in the working copy"),
        "{summary}"
    );
    assert!(summary.contains("  115: 114: let value_114"), "{summary}");
}

#[test]
fn disabled_summarizer_returns_unchanged_signal() {
    let output = "line\n".repeat(400);
    let options = ShellOutputSummaryOptions {
        enabled: false,
        applied_token_limit: Some(400),
        command_text: Some("cargo test"),
    };

    assert_eq!(
        summarize_shell_output_for_model(&output, 1, false, options),
        None
    );
}

#[test]
fn oversized_single_line_retains_bounded_head_and_tail() {
    // The diagnostic label keeps a successful single-line output eligible for
    // summarization; the head and tail of the line must both survive.
    let output = format!(
        "warning: HEAD{}TAIL",
        "x".repeat(DEFAULT_SUMMARY_AFTER_BYTES + 1024)
    );

    let summary =
        summarize_shell_output_for_model(&output, 0, false, options(None, Some(4_000))).unwrap();

    assert!(summary.contains("HEAD"));
    assert!(summary.contains("TAIL"));
    assert!(summary.contains("[line truncated]"));
    assert!(summary.len() <= SUMMARY_MAX_BYTES + "\n[summary capped]".len());
}

#[test]
fn caller_budget_preserves_each_selected_failure_without_retruncation() {
    let mut lines = vec!["ordinary context".to_string(); 900];
    for (index, name) in [
        (100, "FIRST_FAILURE"),
        (350, "MIDDLE_FAILURE"),
        (600, "LAST_FAILURE"),
    ] {
        lines[index] = format!(
            "error: {name} {}",
            "{{\"detail\":\"verbose assertion\"}}".repeat(3000)
        );
        lines[index + 1] = format!("  --> src/{name}.rs:7:3");
    }
    lines[899] = "test result: FAILED. 12 passed; 3 failed".into();
    let raw = lines.join("\n");
    for limit in [2000, 4000] {
        let summary = summarize_shell_output_for_model(
            &raw,
            101,
            false,
            options(Some("cargo test"), Some(limit)),
        )
        .unwrap();
        assert!(
            codex_utils_string::approx_token_count(&summary) <= limit,
            "summary must fit before downstream projection"
        );
        let projected = crate::tools::project_exec_output_for_model_with_budget(
            &codex_protocol::exec_output::ExecToolCallOutput {
                exit_code: 101,
                aggregated_output: codex_protocol::exec_output::StreamOutput::new(raw.clone()),
                ..Default::default()
            },
            codex_utils_output_truncation::TruncationPolicy::Tokens(limit),
            Some(limit),
            Some("cargo test"),
        );
        for expected in [
            "FIRST_FAILURE",
            "MIDDLE_FAILURE",
            "LAST_FAILURE",
            "test result: FAILED. 12 passed; 3 failed",
        ] {
            assert!(
                projected.text.contains(expected),
                "missing {expected}: {}",
                projected.text
            );
        }
        assert!(!projected.text.contains("tokens truncated"));
        assert!(projected.reduced);
    }
}

#[test]
fn tiny_single_line_budget_stops_before_split_utf8_character() {
    assert_eq!(summarize_oversized_line("a😀z", 4), "a");
}

#[test]
fn validation_summary_keeps_typescript_and_npm_errors_outside_tail() {
    for (command, diagnostic) in [
        (
            "npx tsc --noEmit",
            "src/app.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.",
        ),
        ("npm run build", "npm ERR! code ELIFECYCLE"),
    ] {
        let mut lines = (0..700)
            .map(|index| format!("ordinary line {index}"))
            .collect::<Vec<_>>();
        lines[200] = diagnostic.to_string();
        let summary = summarize_shell_output_for_model(
            &lines.join("\n"),
            1,
            false,
            options(Some(command), None),
        )
        .expect("large validation summary");
        assert!(summary.contains(diagnostic), "{summary}");
        assert!(
            !summary.contains("ordinary line 0\n"),
            "validation omits routine head"
        );
        assert!(summary.contains("ordinary line 699"));
    }
}

#[test]
fn summary_declines_output_that_would_grow() {
    let output = "\n".repeat(601);
    assert_eq!(
        summarize_shell_output_for_model(&output, 0, false, options(None, None)),
        None
    );
}

#[test]
fn source_reads_get_room_without_expanding_noisy_command_defaults() {
    for command in [
        "cat src/lib.rs",
        "rg -n pattern src",
        "Get-Content src/lib.rs | Select-Object -First 500",
        "rg -n 'fn |impl ' src; Get-Content src/lib.rs | Select-Object -Skip 40 -First 90",
    ] {
        assert_eq!(
            super::source_read_output_budget(command),
            Some(25_000),
            "{command}"
        );
    }
    for command in [
        "cargo test",
        "npm run build",
        "echo huge-output",
        "cat file; cargo test",
    ] {
        assert_eq!(super::source_read_output_budget(command), None, "{command}");
    }
}
