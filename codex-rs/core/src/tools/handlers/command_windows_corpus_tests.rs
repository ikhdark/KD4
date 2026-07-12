use std::path::Path;
use std::process::Command;

use codex_shell_command::shell_detect::PowerShellHostKind;
use serde_json::Value;

use crate::shell::Shell;
use crate::shell::ShellType;
use crate::shell::get_shell;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::command_execution::CommandExecutionLedger;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use crate::tools::handlers::command_preflight::powershell_single_quoted_literal;
use crate::tools::handlers::command_preflight::preflight_invocation_with_equivalent_repair;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::shell_output_summary::ShellOutputSummaryOptions;
use crate::tools::shell_output_summary::summarize_shell_output_for_model;

fn run(
    invocation: &CommandInvocation,
    shell: &Shell,
    cwd: &Path,
) -> std::io::Result<std::process::Output> {
    let command = invocation.to_exec_args(shell, /*use_login_shell*/ false);
    Command::new(&command[0])
        .args(&command[1..])
        .current_dir(cwd)
        .output()
}

#[tokio::test]
async fn windows_command_corpus_measures_phase2_exit_gate() {
    let temp = tempfile::tempdir().expect("tempdir");
    let unicode_root = temp.path().join("space snow 雪");
    std::fs::create_dir_all(&unicode_root).expect("create Unicode corpus root");

    let python = which::which("python")
        .or_else(|_| which::which("python3"))
        .expect("Windows KD4 validation requires Python");
    let dummy_shell = Shell {
        shell_type: ShellType::Cmd,
        shell_path: "cmd.exe".into(),
    };
    let long_path_argument = format!(r"C:\{}\file.txt", "segment".repeat(42));
    let exact_arguments = vec![
        "space value".to_string(),
        "snow 雪".to_string(),
        "\"quoted\"".to_string(),
        "*.rs".to_string(),
        "a>b".to_string(),
        "x|y".to_string(),
        long_path_argument.clone(),
    ];
    let mut python_args = vec![
        "-c".to_string(),
        "import json,sys; print(json.dumps(sys.argv[1:]))".to_string(),
    ];
    python_args.extend(exact_arguments.clone());
    let argv_invocation = CommandInvocation::Argv {
        program: python.to_string_lossy().into_owned(),
        args: python_args,
    };
    let argv_output = run(&argv_invocation, &dummy_shell, temp.path())
        .expect("execute exact-argv corpus command");
    assert!(
        argv_output.status.success(),
        "exact argv failed: {}",
        String::from_utf8_lossy(&argv_output.stderr)
    );
    let argv_round_trip: Vec<String> =
        serde_json::from_slice(&argv_output.stdout).expect("parse argv round trip");
    assert_eq!(argv_round_trip, exact_arguments);

    let nonzero_invocation = CommandInvocation::Argv {
        program: python.to_string_lossy().into_owned(),
        args: vec!["-c".to_string(), "raise SystemExit(7)".to_string()],
    };
    let nonzero_output = run(&nonzero_invocation, &dummy_shell, temp.path())
        .expect("execute nonzero-exit corpus command");
    assert_eq!(nonzero_output.status.code(), Some(7));

    let powershell = get_shell(ShellType::PowerShell, /*path*/ None)
        .expect("Windows must provide a PowerShell compatibility host");
    let host_kind =
        codex_shell_command::shell_detect::powershell_host_kind(powershell.shell_path.as_path())
            .expect("resolved PowerShell host kind");
    if which::which("pwsh").is_ok() {
        assert_eq!(host_kind, PowerShellHostKind::Pwsh);
    }

    let txt_path = unicode_root.join("alpha one.txt");
    let log_path = unicode_root.join("beta.log");
    let redirected_path = unicode_root.join("redirected output.txt");
    let root_literal = powershell_single_quoted_literal(&unicode_root);
    let txt_literal = powershell_single_quoted_literal(&txt_path);
    let log_literal = powershell_single_quoted_literal(&log_path);
    let redirected_literal = powershell_single_quoted_literal(&redirected_path);
    let script = format!(
        "$ErrorActionPreference = 'Stop'\n\
         Set-Content -LiteralPath {txt_literal} -Value 'snow 雪'\n\
         Set-Content -LiteralPath {log_literal} -Value 'log'\n\
         'redirect ok' > {redirected_literal}\n\
         $sum = (1, 2, 3 | Measure-Object -Sum).Sum\n\
         $wildcards = @(Get-ChildItem -LiteralPath {root_literal} -Filter '*.txt').Count\n\
         [ordered]@{{\n\
           text = (Get-Content -LiteralPath {txt_literal})\n\
           redirected = (Get-Content -LiteralPath {redirected_literal})\n\
           sum = $sum\n\
           wildcard_count = $wildcards\n\
           quoted = '\"quoted\"'\n\
           long_path_length = {long_path_length}\n\
         }} | ConvertTo-Json -Compress",
        long_path_length = long_path_argument.len(),
    );
    let powershell_invocation = CommandInvocation::PowerShellScript(script.clone());
    let encoded_args = powershell_invocation.to_exec_args(&powershell, false);
    assert!(encoded_args.iter().any(|arg| arg == "-EncodedCommand"));
    assert!(!encoded_args.iter().any(|arg| arg == &script));
    let powershell_output = run(&powershell_invocation, &powershell, temp.path())
        .expect("execute encoded PowerShell corpus command");
    assert!(
        powershell_output.status.success(),
        "encoded PowerShell failed: {}",
        String::from_utf8_lossy(&powershell_output.stderr)
    );
    let powershell_json: Value =
        serde_json::from_slice(&powershell_output.stdout).expect("parse PowerShell JSON");
    assert_eq!(powershell_json["text"], "snow 雪");
    assert_eq!(powershell_json["redirected"], "redirect ok");
    assert_eq!(powershell_json["sum"].as_f64(), Some(6.0));
    assert_eq!(powershell_json["wildcard_count"].as_u64(), Some(2));
    assert_eq!(powershell_json["quoted"], "\"quoted\"");
    assert_eq!(
        powershell_json["long_path_length"].as_u64(),
        Some(long_path_argument.len() as u64)
    );

    let repair_source = CommandInvocation::Argv {
        program: "rg".to_string(),
        args: vec!["--ignorecase".to_string(), "TODO".to_string()],
    };
    let repair = preflight_invocation_with_equivalent_repair(
        &repair_source,
        &repair_source.to_direct_argv().expect("direct argv"),
        None,
    )
    .expect("read-only equivalent repair");
    assert!(repair.repaired());

    let mutating_source = CommandInvocation::Argv {
        program: "git".to_string(),
        args: vec!["--worktree".to_string(), "status".to_string()],
    };
    assert!(
        preflight_invocation_with_equivalent_repair(
            &mutating_source,
            &mutating_source.to_direct_argv().expect("direct argv"),
            None,
        )
        .is_err()
    );

    let ledger = CommandExecutionLedger::default();
    let failure_key = CommandAttemptKey::new(
        "exec_command",
        "local",
        temp.path().to_string_lossy().into_owned(),
        &["native-failure".to_string()],
    );
    ledger
        .begin_attempt(&failure_key, false)
        .await
        .expect("first failed attempt");
    ledger.record_exit(&failure_key, 7).await;
    ledger
        .begin_attempt(&failure_key, false)
        .await
        .expect("second failed attempt");
    ledger.record_exit(&failure_key, 7).await;
    assert!(ledger.begin_attempt(&failure_key, false).await.is_err());

    let raw_output = (0..1_000)
        .map(|index| {
            if index == 500 {
                format!("error: retained marker {index}")
            } else {
                format!("ordinary output line {index}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let summary = summarize_shell_output_for_model(
        &raw_output,
        1,
        false,
        ShellOutputSummaryOptions {
            enabled: true,
            turn_cost_guard: false,
            command_text: Some("cargo test"),
        },
    )
    .expect("large output should be summarized");
    assert!(summary.contains("error: retained marker 500"));
    let output_reduction_ratio = summary.len() as f64 / raw_output.len() as f64;
    assert!(output_reduction_ratio < 0.5);

    let artifact =
        create_raw_output_artifact(temp.path(), "corpus-thread", raw_output.as_bytes()).await;
    let RawOutputArtifact::Stored { path, bytes } = artifact else {
        panic!("raw output artifact should be stored");
    };
    assert_eq!(bytes, raw_output.len() as u64);
    assert_eq!(
        tokio::fs::read(path).await.expect("read corpus artifact"),
        raw_output.as_bytes()
    );

    let metrics = serde_json::json!({
        "schema_version": 1,
        "first_attempt_success": {
            "exact_argv": argv_output.status.success(),
            "encoded_powershell": powershell_output.status.success(),
        },
        "repair_applied": repair.repaired(),
        "mutating_auto_repair": false,
        "repeated_failure_blocked_on_attempt": 3,
        "powershell_host": {
            "kind": format!("{host_kind:?}"),
            "path": powershell.shell_path,
        },
        "output": {
            "raw_bytes": raw_output.len(),
            "model_bytes": summary.len(),
            "reduction_ratio": output_reduction_ratio,
            "artifact_bytes": bytes,
        },
        "coverage": [
            "spaces",
            "unicode",
            "quoting",
            "wildcards",
            "redirection",
            "pipelines",
            "long_paths",
            "nonzero_native_exit",
        ],
    });
    tracing::info!(
        target: "kd4_windows_command_corpus",
        metrics = %metrics,
        "measured Phase 2 Windows command corpus"
    );
}
