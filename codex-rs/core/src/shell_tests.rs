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
        let args = shell
            .derive_exec_args(command, /*use_login_shell*/ false)
            .expect("Windows shell must derive execution arguments");
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
        test_powershell_shell
            .derive_exec_args("echo hello", /*use_login_shell*/ false)
            .expect("PowerShell args"),
        vec!["pwsh.exe", "-NoProfile", "-Command", "echo hello"]
    );
    assert_eq!(
        test_powershell_shell
            .derive_exec_args("echo hello", /*use_login_shell*/ true)
            .expect("PowerShell args"),
        vec!["pwsh.exe", "-Command", "echo hello"]
    );
}

#[test]
fn compatibility_shells_cannot_derive_execution_arguments() {
    for shell_type in [ShellType::Bash, ShellType::Zsh, ShellType::Sh] {
        let shell = Shell {
            shell_type,
            shell_path: PathBuf::from(shell_type.name()),
        };
        let error = shell
            .derive_exec_args("echo hello", /*use_login_shell*/ false)
            .expect_err("compatibility-only shell must not execute");
        assert!(error.to_string().contains("cannot execute on Windows"));
    }
}

#[test]
fn compatibility_environment_shells_are_rejected() {
    for name in ["bash", "zsh", "sh"] {
        let error = Shell::from_environment_shell_info(ShellInfo {
            name: name.to_string(),
            path: format!("/{name}"),
        })
        .expect_err("compatibility-only remote shell must not be selected");
        assert!(error.to_string().contains("cannot execute on Windows"));
    }
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
