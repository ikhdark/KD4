//! Shell output summaries as projected through codex-core's exec output paths.
use crate::tools::shell_output_summary::*;

fn compiler_record(message: &str) -> String {
    serde_json::json!({"reason":"compiler-message", "message": {
        "level":"error", "message":message,
        "code":{"code":"E0308", "explanation":"manual ".repeat(1000)},
        "spans":[{"file_name":"src/λ.rs", "line_start":7, "column_start":3,
            "is_primary":true, "suggested_replacement":"value.into()", "suggestion_applicability":"MachineApplicable"}],
        "children":[{"level":"help", "message":"convert the value", "spans":[], "children":[]}],
        "rendered":"redundant compiler prose ".repeat(3000)
    }}).to_string()
}

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
fn structured_diagnostics_replace_the_lossy_summary_and_keep_fixes() {
    let output = format!(
        "{}\n{}\n{}\n",
        compiler_record("expected λ"),
        serde_json::json!({"reason":"build-finished","success":false}),
        serde_json::json!({"reason":"build-finished","success":true})
    );
    let text = summarize_shell_output_for_model(
        &output,
        1,
        false,
        options(Some("cargo check --message-format=json"), Some(2000)),
    )
    .unwrap();
    let result: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(result["format"], "compiler_diagnostics");
    assert_eq!(result["build_success"], false);
    assert_eq!(result["diagnostics_complete"], true);
    assert_eq!(result["diagnostics"][0]["source_line"], 1);
    let diagnostic = &result["diagnostics"][0]["diagnostic"];
    assert_eq!(diagnostic["message"], "expected λ");
    assert_eq!(
        diagnostic["spans"][0]["suggested_replacement"],
        "value.into()"
    );
    assert_eq!(diagnostic["children"][0]["message"], "convert the value");
    assert!(diagnostic.get("rendered").is_none());
    assert!(!text.contains("Shell output summary:"));
    let exec_output = codex_protocol::exec_output::ExecToolCallOutput {
        exit_code: 1,
        stdout: codex_protocol::exec_output::StreamOutput::new(output.clone()),
        stderr: codex_protocol::exec_output::StreamOutput::new(String::new()),
        aggregated_output: codex_protocol::exec_output::StreamOutput::new(output.clone()),
        duration: std::time::Duration::ZERO,
        timed_out: false,
    };
    let projected = crate::tools::project_exec_output_text_with_budget(
        &exec_output,
        codex_utils_output_truncation::TruncationPolicy::Tokens(2000),
        Some(2000),
        Some("cargo check --message-format=json"),
    );
    assert!(projected.reduced);
    assert!(projected.text.contains("compiler_diagnostics"));
    assert!(projected.text.contains("suggested_replacement"));
    assert_eq!(exec_output.aggregated_output.text, output);
    // Source reads must never be interpreted as compiler diagnostics.
    assert!(
        summarize_shell_output_for_model(
            &output,
            0,
            false,
            options(Some("cat output.jsonl"), Some(2000))
        )
        .is_none()
    );
}

#[test]
fn verified10_compiler_recovery_uses_canonical_crlf_and_unicode_bytes() {
    use crate::tools::context::{ExecCommandToolOutput, ToolOutput, ToolPayload};
    use crate::unified_exec::ProcessOutputSnapshot;
    use codex_utils_output_truncation::TruncationPolicy;
    use std::sync::Arc;
    let record = compiler_record(&"long λ detail ".repeat(4000));
    let prefix = "building λ\r\n";
    let stdout = format!("{prefix}{record}\r\n");
    for interleaved in [false, true] {
        let stderr = "stderr λ\r\n";
        let split = if interleaved {
            prefix.len() + record.find("rendered").unwrap()
        } else {
            prefix.len()
        };
        let raw = format!("{}{stderr}{}", &stdout[..split], &stdout[split..]);
        let output = ExecCommandToolOutput {
            output_ranges: None,
            process_output: Some(Arc::new(ProcessOutputSnapshot {
                aggregated_output: raw.as_bytes().to_vec(),
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                aggregated_output_is_exact: true,
                streams_are_exact: true,
            })),
            error: None,
            validation: None,
            event_call_id: String::new(),
            chunk_id: String::new(),
            wall_time: std::time::Duration::ZERO,
            raw_output: raw.as_bytes().to_vec(),
            truncation_policy: TruncationPolicy::Tokens(2000),
            max_output_tokens: Some(2000),
            process_id: None,
            session_capabilities: None,
            exit_code: Some(1),
            process_exited: true,
            search_no_match: false,
            original_token_count: None,
            hook_command: Some("cargo check --message-format=json".into()),
            raw_output_artifact: None,
            repair_notice: None,
            pending_deferred_completions: Vec::new(),
        };
        let result = output.code_mode_result(&ToolPayload::Function {
            arguments: "{}".into(),
        });
        let summary: serde_json::Value =
            serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
        assert_eq!(summary["diagnostic_stream"], "stdout");
        assert_eq!(summary["stderr"], stderr);
        assert_eq!(summary["original_bytes"], raw.len());
        let selector = &summary["diagnostics"][0]["recovery_selector"];
        let bytes = &raw[selector["start"].as_u64().unwrap() as usize
            ..selector["end"].as_u64().unwrap() as usize];
        assert_eq!(
            bytes,
            if interleaved {
                raw.as_str()
            } else {
                record.as_str()
            }
        );
    }
}

#[test]
fn compiler_json_uses_complete_stdout_and_keeps_stderr_independent() {
    use codex_protocol::exec_output::{ExecToolCallOutput, StreamOutput};
    use codex_utils_output_truncation::TruncationPolicy;

    let stdout = compiler_record("authoritative stdout failure");
    // Reader scheduling can interleave stderr inside a stdout JSON record.
    let stderr = "{stderr progress, not JSON}\n   Compiling sample\r\n";
    let split = stdout.find("rendered").unwrap();
    let mixed = format!("{}{}{}", &stdout[..split], stderr, &stdout[split..]);
    let mut output = ExecToolCallOutput {
        exit_code: 1,
        stdout: StreamOutput::new(stdout),
        stderr: StreamOutput::new(stderr.to_string()),
        aggregated_output: StreamOutput::new(mixed.clone()),
        ..Default::default()
    };
    let project = |output: &ExecToolCallOutput| {
        crate::tools::project_exec_output_text_with_budget(
            output,
            TruncationPolicy::Tokens(2000),
            Some(2000),
            Some("cargo check --message-format=json"),
        )
    };
    let projected = project(&output);
    let summary: serde_json::Value = serde_json::from_str(&projected.text).unwrap();
    assert!(projected.reduced);
    assert_eq!(summary["diagnostic_stream"], "stdout");
    assert_eq!(summary["stderr"], stderr);
    assert_eq!(summary["diagnostics"][0]["source_line"], 1);
    assert_eq!(
        summary["diagnostics"][0]["diagnostic"]["message"],
        "authoritative stdout failure"
    );
    assert_eq!(
        summary["diagnostics"][0]["diagnostic"]["spans"][0]["suggested_replacement"],
        "value.into()"
    );
    let direct = crate::tools::project_exec_output_for_model_with_budget(
        &output,
        TruncationPolicy::Tokens(2000),
        Some(2000),
        Some("cargo check --message-format=json"),
    );
    assert!(
        direct.text.contains("compiler_diagnostics") && direct.text.contains("stderr progress")
    );
    assert_eq!(output.aggregated_output.text, mixed);

    for stream in 0..3 {
        let mut incomplete = output.clone();
        match stream {
            0 => incomplete.stdout.truncated = true,
            1 => incomplete.stderr.truncated_after_lines = Some(1),
            _ => incomplete.aggregated_output.truncated = true,
        }
        assert!(
            !project(&incomplete)
                .text
                .contains("\"format\":\"compiler_diagnostics\"")
        );
    }
    output
        .stdout
        .text
        .push_str("\n{\"reason\":\"unknown-output\"}");
    output
        .aggregated_output
        .text
        .push_str("\n{\"reason\":\"unknown-output\"}");
    let unknown = project(&output);
    assert!(!unknown.text.contains("\"format\":\"compiler_diagnostics\""));
    assert!(unknown.text.contains("unknown-output"), "{}", unknown.text);
}

#[test]
fn rustc_stderr_diagnostics_keep_constraints_through_both_output_paths() {
    use crate::tools::context::{ExecCommandToolOutput, ToolOutput, ToolPayload};
    use crate::unified_exec::ProcessOutputSnapshot;
    use codex_protocol::exec_output::{ExecToolCallOutput, StreamOutput};
    use codex_utils_output_truncation::TruncationPolicy;
    use std::sync::Arc;

    // rustc --error-format=json emits native diagnostic records on stderr,
    // unlike Cargo's compiler-message envelopes on stdout.
    let cargo: serde_json::Value =
        serde_json::from_str(&compiler_record("borrow of moved value")).unwrap();
    let mut diagnostic = cargo["message"].clone();
    diagnostic["$message_type"] = "diagnostic".into();
    diagnostic["code"]["code"] = "E0382".into();
    diagnostic["spans"][0]["suggested_replacement"] = serde_json::Value::Null;
    diagnostic["spans"][0]["suggestion_applicability"] = serde_json::Value::Null;
    diagnostic["children"] = serde_json::json!([{
        "level":"help", "message":"consider cloning the value if the performance cost is acceptable",
        "spans":[{"file_name":"src/λ.rs", "line_start":6, "column_start":8,
            "is_primary":true, "suggested_replacement":".clone()",
            "suggestion_applicability":"MachineApplicable"}], "children":[]
    }]);
    diagnostic["spans"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "file_name":"src/λ.rs", "line_start":6, "column_start":3,
            "is_primary":false, "label":"value moved here"
        }));
    let stderr = format!("{diagnostic}\r\n");
    let output = ExecToolCallOutput {
        exit_code: 1,
        stdout: StreamOutput::new(String::new()),
        stderr: StreamOutput::new(stderr.clone()),
        aggregated_output: StreamOutput::new(stderr.clone()),
        ..Default::default()
    };
    let project = |output: &ExecToolCallOutput| {
        crate::tools::project_exec_output_text_with_budget(
            output,
            TruncationPolicy::Tokens(2000),
            Some(2000),
            Some("rustc --error-format=json"),
        )
    };
    let assert_constraints = |text: &str| {
        let summary: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(summary["diagnostic_stream"], "stderr");
        assert_eq!(summary["error_count"], 1);
        assert_eq!(summary["stderr"], serde_json::Value::Null);
        assert_eq!(summary["diagnostics_complete"], true);
        let kept = &summary["diagnostics"][0]["diagnostic"];
        assert_eq!(kept["code"]["code"], "E0382");
        assert_eq!(kept["spans"], diagnostic["spans"]);
        assert_eq!(kept["children"], diagnostic["children"]);
        assert!(kept.get("rendered").is_none());
        assert!(kept["code"].get("explanation").is_none());
    };
    assert_constraints(&project(&output).text);
    assert_eq!(output.stderr.text, stderr);
    for stream in 0..3 {
        let mut incomplete = output.clone();
        match stream {
            0 => incomplete.stdout.truncated = true,
            1 => incomplete.stderr.truncated = true,
            _ => incomplete.aggregated_output.truncated = true,
        }
        // A valid captured aggregate may still be projected, but rejected
        // producer streams must not acquire authoritative stderr attribution.
        if let Ok(summary) = serde_json::from_str::<serde_json::Value>(&project(&incomplete).text) {
            assert_eq!(summary["diagnostic_stream"], "aggregated");
        }
    }
    let mut partial = output.clone();
    let cut = stderr.find("rendered").unwrap();
    partial.stderr.text.truncate(cut);
    partial.stderr.truncated = true;
    partial.aggregated_output.text.truncate(cut);
    partial.aggregated_output.truncated = true;
    assert!(
        !project(&partial)
            .text
            .contains("\"format\":\"compiler_diagnostics\"")
    );
    for suffix in ["\n{broken", "\n{\"reason\":\"unknown-output\"}"] {
        let mut unknown = output.clone();
        unknown.stderr.text.push_str(suffix);
        unknown.aggregated_output.text.push_str(suffix);
        assert!(
            !project(&unknown)
                .text
                .contains("\"format\":\"compiler_diagnostics\"")
        );
    }
    // Do not silently drop another stream or reinterpret printed source JSON.
    let mut mixed = output.clone();
    mixed.stdout.text = "other command output\n".into();
    mixed.aggregated_output.text.push_str(&mixed.stdout.text);
    assert!(
        !project(&mixed)
            .text
            .contains("\"format\":\"compiler_diagnostics\"")
    );
    assert!(
        summarize_shell_output_for_model_with_streams(
            &stderr,
            1,
            false,
            options(Some("cat diagnostics.json"), Some(2000)),
            Some(("", &stderr))
        )
        .is_none()
    );

    let mut unified = ExecCommandToolOutput {
        output_ranges: None,
        process_output: Some(Arc::new(ProcessOutputSnapshot {
            aggregated_output: stderr.as_bytes().to_vec(),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
            aggregated_output_is_exact: true,
            streams_are_exact: true,
        })),
        error: None,
        validation: None,
        event_call_id: String::new(),
        chunk_id: String::new(),
        wall_time: std::time::Duration::ZERO,
        raw_output: stderr.into_bytes(),
        truncation_policy: TruncationPolicy::Tokens(2000),
        max_output_tokens: Some(2000),
        process_id: None,
        session_capabilities: None,
        exit_code: Some(1),
        process_exited: true,
        search_no_match: false,
        original_token_count: None,
        hook_command: Some("rustc --error-format=json".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    assert_constraints(
        unified.code_mode_result(&payload)["output"]
            .as_str()
            .unwrap(),
    );
    Arc::make_mut(unified.process_output.as_mut().unwrap()).streams_are_exact = false;
    let inexact = unified.code_mode_result(&payload);
    if let Ok(summary) =
        serde_json::from_str::<serde_json::Value>(inexact["output"].as_str().unwrap())
    {
        assert_eq!(summary["diagnostic_stream"], "aggregated");
    }
    Arc::make_mut(unified.process_output.as_mut().unwrap()).streams_are_exact = true;
    unified.raw_output = "last poll only\n".repeat(1000).into_bytes();
    assert!(
        !unified.code_mode_result(&payload)["output"]
            .as_str()
            .unwrap()
            .contains("compiler_diagnostics")
    );
}

#[test]
fn unified_exec_compiler_streams_do_not_replay_prior_poll_output() {
    use crate::tools::context::{ExecCommandToolOutput, ToolOutput, ToolPayload};
    use crate::unified_exec::ProcessOutputSnapshot;
    use codex_utils_output_truncation::TruncationPolicy;
    use std::sync::Arc;

    let stdout = compiler_record("unified stdout failure");
    let stderr = "{stderr progress, not JSON}\r\n";
    let split = stdout.find("rendered").unwrap();
    let mixed = format!("{}{}{}", &stdout[..split], stderr, &stdout[split..]);
    let mut output = ExecCommandToolOutput {
        output_ranges: None,
        process_output: Some(Arc::new(ProcessOutputSnapshot {
            aggregated_output: mixed.as_bytes().to_vec(),
            stdout: stdout.into_bytes(),
            stderr: stderr.as_bytes().to_vec(),
            aggregated_output_is_exact: true,
            streams_are_exact: true,
        })),
        error: None,
        validation: None,
        event_call_id: String::new(),
        chunk_id: String::new(),
        wall_time: std::time::Duration::ZERO,
        raw_output: mixed.into_bytes(),
        truncation_policy: TruncationPolicy::Tokens(2000),
        max_output_tokens: Some(2000),
        process_id: None,
        session_capabilities: None,
        exit_code: Some(1),
        process_exited: true,
        search_no_match: false,
        original_token_count: None,
        hook_command: Some("cargo check --message-format=json".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let result = output.code_mode_result(&payload);
    let summary: serde_json::Value =
        serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
    assert_eq!(
        summary["diagnostics"][0]["diagnostic"]["message"],
        "unified stdout failure"
    );
    assert_eq!(summary["stderr"], stderr);
    Arc::make_mut(output.process_output.as_mut().unwrap()).streams_are_exact = false;
    assert!(
        !output.code_mode_result(&payload)["output"]
            .as_str()
            .unwrap()
            .contains("compiler_diagnostics")
    );
    Arc::make_mut(output.process_output.as_mut().unwrap()).streams_are_exact = true;
    output.raw_output = b"last poll only\n".to_vec();
    assert_eq!(
        output.code_mode_result(&payload)["output"],
        "last poll only\n"
    );
    // A chunk under the budget is returned before the stream check runs. Only
    // an over-budget chunk shows that the cumulative streams are not replayed.
    output.raw_output = "last poll only\n".repeat(1_000).into_bytes();
    let over_budget = output.code_mode_result(&payload)["output"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(over_budget.contains("last poll only"), "{over_budget}");
    assert!(
        !over_budget.contains("compiler_diagnostics"),
        "{over_budget}"
    );
    assert!(
        !over_budget.contains("unified stdout failure"),
        "{over_budget}"
    );
    output.raw_output.clear();
    assert_eq!(output.code_mode_result(&payload)["output"], "");
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
fn selected_errors_do_not_spend_the_final_status_quota() {
    let mut lines = (0..900)
        .map(|index| format!("ordinary line {index}"))
        .collect::<Vec<_>>();
    for index in 0..8 {
        lines[50 + index * 10] = format!("suite {index} passed; KEEP_STATUS_{index}");
        // `error:` lines are status lines as well as diagnostics, so only the
        // already-selected check keeps them out of the eight status slots.
        lines[300 + index * 10] = format!("error: KEEP_ERROR_{index}");
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
fn dense_output_over_token_budget_keeps_middle_diagnostics() {
    let mut lines = vec!["{}[]():,;".repeat(5); 500];
    lines[250] = "error: unique middle diagnostic".to_string();
    lines[499] = "test result: FAILED".to_string();
    let output = lines.join("\n");
    let limit = 4000;
    assert!(output.len() > codex_utils_string::approx_bytes_for_tokens(limit));
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
