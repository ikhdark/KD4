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
    assert!(summary.len() <= SUMMARY_MAX_BYTES);
    assert!(summary.lines().count() <= SUMMARY_MAX_LINES);
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
fn oversized_head_lines_do_not_crowd_out_the_failure() {
    let mut lines = (0..700)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>();
    for line in lines.iter_mut().take(SUCCESS_HEAD_LINES) {
        *line = format!("HEAD{}TAIL", "x".repeat(4_000));
    }
    lines[175] = "error[E0425]: keep this diagnostic".to_string();
    let output = lines.join("\n");

    let summary =
        summarize_shell_output_for_model(&output, 1, false, options(None, false)).unwrap();

    assert!(summary.contains("error[E0425]: keep this diagnostic"));
    assert!(summary.len() <= SUMMARY_MAX_BYTES);
    assert!(summary.lines().count() <= SUMMARY_MAX_LINES);
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
fn validation_commands_enable_the_early_cost_guard() {
    let output = (0..200)
        .map(|index| format!("validation line {index}"))
        .collect::<Vec<_>>()
        .join("\n");

    let summary = summarize_shell_output_for_model(
        &output,
        0,
        false,
        options(Some("cargo test -p codex-core"), false),
    )
    .expect("diagnostic output should summarize at the guarded threshold");

    assert!(summary.contains("validation line 199"));
    assert!(summary.len() <= SUMMARY_MAX_BYTES);
    assert!(summary.lines().count() <= SUMMARY_MAX_LINES);
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
    assert!(summary.len() <= SUMMARY_MAX_BYTES);
}

#[test]
fn cap_marker_is_inside_strict_byte_and_line_limits() {
    let mut builder = SummaryBuilder::new();
    for index in 0..SUMMARY_MAX_LINES {
        builder.push_line(format!("{index:03} {}", "é".repeat(200)));
    }
    builder.push_line("forces the capped marker");

    let summary = builder.finish().expect("summary should not be empty");
    assert!(summary.ends_with("[summary capped]"));
    assert!(summary.len() <= SUMMARY_MAX_BYTES);
    assert!(summary.lines().count() <= SUMMARY_MAX_LINES);
    assert!(summary.is_char_boundary(summary.len()));
}
