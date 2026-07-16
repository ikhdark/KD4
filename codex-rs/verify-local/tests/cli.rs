use std::path::Path;
use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_codex-verify-local")
}

fn repository_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace repository root")
}

#[test]
fn help_is_stdout_only_and_argument_errors_are_stderr_only() {
    let help = Command::new(binary()).arg("--help").output().expect("help");
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert!(help.stdout.ends_with(b"\n"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage:"));

    let error = Command::new(binary())
        .arg("--definitely-not-a-verifier-option")
        .output()
        .expect("argument error");
    assert_eq!(error.status.code(), Some(2));
    assert!(error.stdout.is_empty());
    assert!(String::from_utf8_lossy(&error.stderr).contains("unexpected argument"));
}

#[test]
fn domain_results_are_stdout_only_with_exact_native_newline_and_exit_code() {
    let planned = Command::new(binary())
        .args([
            "--plan",
            "--json",
            "--changed",
            "codex-rs/verify-local/src/lib.rs",
            "--repository-root",
        ])
        .arg(repository_root())
        .output()
        .expect("planned result");
    assert_eq!(planned.status.code(), Some(0));
    assert!(planned.stderr.is_empty());
    let expected_newline: &[u8] = if cfg!(windows) { b"}\r\n" } else { b"}\n" };
    assert!(planned.stdout.ends_with(expected_newline));
    assert!(String::from_utf8_lossy(&planned.stdout).contains("\"verdict\": \"PLANNED\""));

    let tooling_error = Command::new(binary())
        .args(["--scope-start", "scope", "--repository-root"])
        .arg(repository_root())
        .output()
        .expect("domain error");
    assert_eq!(tooling_error.status.code(), Some(4));
    assert!(tooling_error.stderr.is_empty());
    assert!(
        String::from_utf8_lossy(&tooling_error.stdout).contains("\"verdict\": \"TOOLING_ERROR\"")
    );
}
