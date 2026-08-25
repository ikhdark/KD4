use super::*;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn can_run_on_shell_test() {
    let cmd = "echo \"Works\"";
    assert!(shell_works(
        get_shell(ShellType::PowerShell, /*path*/ None),
        "Write-Output 'Works'",
        /*required*/ true,
    ));
    assert!(shell_works(
        get_shell(ShellType::Cmd, /*path*/ None),
        cmd,
        /*required*/ true,
    ));
    assert!(shell_works(
        Some(ultimate_fallback_shell()),
        cmd,
        /*required*/ true
    ));
}

fn shell_works(shell: Option<Shell>, command: &str, required: bool) -> bool {
    if let Some(shell) = shell {
        let args = shell.derive_exec_args(command, /*use_login_shell*/ false);
        let output = Command::new(args[0].clone())
            .args(&args[1..])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("Works"));
        true
    } else {
        !required
    }
}

#[test]
fn derive_exec_args() {
    let test_powershell_shell = Shell {
        shell_type: ShellType::PowerShell,
        shell_path: PathBuf::from("pwsh.exe"),
    };
    assert_eq!(
        test_powershell_shell.derive_exec_args("echo hello", /*use_login_shell*/ false),
        vec!["pwsh.exe", "-NoProfile", "-Command", "echo hello"]
    );
    assert_eq!(
        test_powershell_shell.derive_exec_args("echo hello", /*use_login_shell*/ true),
        vec!["pwsh.exe", "-Command", "echo hello"]
    );
}

#[test]
fn rejects_model_provided_non_windows_shells() {
    let err = get_shell_by_model_provided_path(&PathBuf::from("bash"))
        .expect_err("bash must not be selectable in the Windows-only runtime");
    assert!(err.to_string().contains("unsupported Windows shell"));
}

#[tokio::test]

async fn detects_powershell_as_default() {
    let powershell_shell = default_user_shell();
    let shell_path = powershell_shell.shell_path;

    assert!(shell_path.ends_with("pwsh.exe") || shell_path.ends_with("powershell.exe"));
}

#[test]

fn finds_powershell() {
    let powershell_shell = get_shell(ShellType::PowerShell, /*path*/ None).unwrap();
    let shell_path = powershell_shell.shell_path;

    assert!(shell_path.ends_with("pwsh.exe") || shell_path.ends_with("powershell.exe"));
}
