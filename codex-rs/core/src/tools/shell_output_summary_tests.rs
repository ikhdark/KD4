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
    assert!(summary.contains("- omitted_lines:"));
    assert!(summary.contains("- omitted_bytes:"));
    assert!(summary.len() <= SUMMARY_MAX_BYTES + "[summary capped]".len() + 1);
    assert!(summary.lines().count() <= SUMMARY_MAX_LINES + 1);
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

    let reduction = reduce_shell_output_for_model(&output, 0, false, options(None, false)).unwrap();
    let summary = reduction.summary;

    assert!(summary.contains("HEAD"));
    assert!(summary.contains("TAIL"));
    assert!(summary.contains("[line truncated]"));
    assert!(reduction.omitted_bytes > 0);
    assert_eq!(reduction.omitted_lines, 0);
    assert!(summary.len() <= SUMMARY_MAX_BYTES + "\n[summary capped]".len());
}

#[test]
fn reducer_cap_counts_only_source_lines_the_builder_accepts() {
    let mut lines = (0..1_000)
        .map(|index| format!("ordinary line {index:04}"))
        .collect::<Vec<_>>();
    for index in (10..730).step_by(15).take(MAX_FOCUS_MATCHES) {
        lines[index] = format!("error: focused failure {index:04}");
    }
    let output = lines.join("\n");

    let reduction = reduce_shell_output_for_model(&output, 1, false, options(None, false)).unwrap();
    assert!(reduction.summary.contains("[summary capped]"));
    let retained_indexes = reduction
        .summary
        .lines()
        .filter_map(|line| line.get(..5)?.trim().parse::<usize>().ok())
        .map(|number| number - 1)
        .collect::<Vec<_>>();
    let source_lines = output.split_inclusive('\n').collect::<Vec<_>>();
    let retained_bytes = retained_indexes
        .iter()
        .map(|index| source_lines[*index].len())
        .sum::<usize>();

    assert_eq!(
        reduction.omitted_lines,
        output
            .lines()
            .count()
            .saturating_sub(retained_indexes.len())
    );
    assert_eq!(
        reduction.omitted_bytes,
        output.len().saturating_sub(retained_bytes)
    );
}
