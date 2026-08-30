use super::*;

fn options(command_text: Option<&str>, turn_cost_guard: bool) -> ShellOutputSummaryOptions<'_> {
    ShellOutputSummaryOptions {
        enabled: true,
        turn_cost_guard,
        command_text,
    }
}

#[test]
fn small_output_is_unchanged() {
    let output = "ok\n";

    let summary = summarize_shell_output_for_model(output, 0, false, options(None, false));

    assert_eq!(summary, None);
}

#[test]
fn large_success_output_keeps_head_tail_and_warning_lines() {
    let mut lines = Vec::new();
    for index in 0..700 {
        lines.push(format!("line {index}"));
    }
    lines[200] = "warning: useful warning".to_string();
    let output = lines.join("\n");

    let summary =
        summarize_shell_output_for_model(&output, 0, false, options(None, false)).unwrap();

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

    let summary =
        summarize_shell_output_for_model(&output, 0, false, options(None, false)).unwrap();
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

    let summary =
        summarize_shell_output_for_model(&output, 1, false, options(None, false)).unwrap();

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
        options(Some("cargo test -p codex-core"), false),
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

    let summary =
        summarize_shell_output_for_model(&output, 1, false, options(None, false)).unwrap();

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

    let summary =
        summarize_shell_output_for_model(&output, 1, false, options(None, false)).unwrap();

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
        options(Some("cargo test -p codex-core"), false),
    )
    .unwrap();

    assert!(summary.contains("thread 'parser::tests::keeps_error' panicked"));
    assert!(summary.contains("failures: parser::tests::keeps_error"));
    assert!(summary.contains("test result: FAILED. 12 passed; 1 failed"));
    assert!(summary.contains("test log 699"));
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
        options(Some("cargo test -p codex-core"), false),
    )
    .unwrap();

    assert!(summary.contains("failure-focused lines, final status lines, tail"));
    assert!(summary.contains("FINAL_STATUS_SENTINEL test result: ok"));
}

#[test]
fn turn_cost_guard_uses_earlier_threshold_without_blocking_semantics() {
    let output = (0..200)
        .map(|index| format!("guard line {index}"))
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        summarize_shell_output_for_model(&output, 0, false, options(None, false)),
        None
    );
    assert!(
        summarize_shell_output_for_model(&output, 0, false, options(None, true))
            .unwrap()
            .contains("guard line 199")
    );
}

#[test]
fn disabled_summarizer_returns_unchanged_signal() {
    let output = "line\n".repeat(400);
    let options = ShellOutputSummaryOptions {
        enabled: false,
        turn_cost_guard: true,
        command_text: Some("cargo test"),
    };

    assert_eq!(
        summarize_shell_output_for_model(&output, 1, false, options),
        None
    );
}

#[test]
fn oversized_single_line_retains_bounded_head_and_tail() {
    let output = format!("HEAD{}TAIL", "x".repeat(DEFAULT_SUMMARY_AFTER_BYTES + 1024));

    let summary =
        summarize_shell_output_for_model(&output, 0, false, options(None, false)).unwrap();

    assert!(summary.contains("HEAD"));
    assert!(summary.contains("TAIL"));
    assert!(summary.contains("[line truncated]"));
    assert!(summary.len() <= SUMMARY_MAX_BYTES + "\n[summary capped]".len());
}

#[test]
fn tiny_single_line_budget_stops_before_split_utf8_character() {
    assert_eq!(summarize_oversized_line("a😀z", 4), "a");
}
