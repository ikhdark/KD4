use std::path::PathBuf;

use serde_json::json;

use crate::function_tool::FunctionCallError;
use crate::shell::Shell;
use crate::shell::ShellType;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::handlers::command_shape::powershell_script_failure_advisory;

fn parse(
    script: Option<&str>,
    kind: Option<&str>,
    program: Option<&str>,
    args: Option<&[String]>,
    script_body: Option<&str>,
) -> Result<CommandInvocation, FunctionCallError> {
    CommandInvocation::from_parts(
        "exec_command",
        "cmd",
        script,
        kind,
        program,
        args,
        script_body,
    )
}

#[test]
fn powershell_script_mode_accepts_script_body_only() {
    let invocation = parse(
        None,
        Some("powershell_script"),
        None,
        None,
        Some("Get-ChildItem -Force"),
    )
    .expect("script_body should be accepted");

    assert_eq!(
        invocation,
        CommandInvocation::PowerShellScript("Get-ChildItem -Force".to_string())
    );
    assert!(invocation.is_powershell_script());
    assert_eq!(invocation.display_command(), "Get-ChildItem -Force");
    assert_eq!(
        invocation.hook_input(),
        json!({
            "command": "Get-ChildItem -Force",
            "kind": "powershell_script",
            "script_body": "Get-ChildItem -Force",
        })
    );
}

#[test]
fn argv_mode_accepts_program_and_args_without_script() {
    let args = vec!["--files".to_string(), "codex-rs".to_string()];
    let invocation =
        parse(None, Some("argv"), Some("rg"), Some(&args), None).expect("argv should parse");

    assert_eq!(
        invocation,
        CommandInvocation::Argv {
            program: "rg".to_string(),
            args
        }
    );
    assert!(invocation.is_argv());
    assert_eq!(invocation.display_command(), "rg --files codex-rs");
    assert_eq!(
        invocation.hook_input(),
        json!({
            "command": "rg --files codex-rs",
            "kind": "argv",
            "program": "rg",
            "args": ["--files", "codex-rs"],
        })
    );
}

#[test]
fn argv_hook_rewrite_preserves_structured_shape() {
    let invocation = parse(
        None,
        Some("argv"),
        Some("rg"),
        Some(&["--files".to_string()]),
        None,
    )
    .expect("argv should parse");

    assert_eq!(
        invocation
            .with_updated_hook_input(
                "exec_command",
                &json!({
                    "kind": "argv",
                    "program": "kds",
                    "args": ["--agent", "--", "rg", "--files"],
                }),
            )
            .expect("structured argv rewrite should be accepted"),
        CommandInvocation::Argv {
            program: "kds".to_string(),
            args: vec![
                "--agent".to_string(),
                "--".to_string(),
                "rg".to_string(),
                "--files".to_string(),
            ],
        }
    );
}

#[test]
fn argv_hook_rewrite_rejects_mixed_or_misleading_shapes() {
    let invocation = parse(None, Some("argv"), Some("rg"), None, None).expect("argv should parse");

    let changed_text = invocation
        .with_updated_hook_input("exec_command", &json!({ "command": "rg --hidden" }))
        .expect_err("changed text must not replace argv");
    assert!(
        changed_text
            .to_string()
            .contains("cannot rewrite a direct argv command as text")
    );

    let conflicting_display = invocation
        .with_updated_hook_input(
            "exec_command",
            &json!({
                "command": "not-the-command",
                "kind": "argv",
                "program": "rg",
                "args": ["--files"],
            }),
        )
        .expect_err("structured argv display must be consistent");
    assert!(
        conflicting_display
            .to_string()
            .contains("does not match its structured `program`/`args`")
    );
}

#[test]
fn untagged_command_preserves_legacy_compatibility() {
    let invocation = parse(Some("rg --files"), None, None, None, None)
        .expect("untagged command should remain compatible");

    assert_eq!(
        invocation,
        CommandInvocation::Script("rg --files".to_string())
    );
}

#[test]
fn tagged_script_and_legacy_shapes_are_explicit() {
    let script =
        parse(Some("Write-Output ok"), Some("script"), None, None, None).expect("tagged script");
    let legacy =
        parse(Some("Write-Output ok"), Some("legacy"), None, None, None).expect("tagged legacy");

    assert_eq!(
        script,
        CommandInvocation::Script("Write-Output ok".to_string())
    );
    assert_eq!(
        legacy,
        CommandInvocation::Script("Write-Output ok".to_string())
    );
}

#[test]
fn script_mode_preserves_exact_nonblank_body() {
    let script_body = "printf '<%s>\\n' foo\\ ";
    let invocation = parse(Some(script_body), Some("script"), None, None, None)
        .expect("nonblank script should be accepted without normalization");
    let shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("bash"),
    };

    assert_eq!(
        invocation,
        CommandInvocation::Script(script_body.to_string())
    );
    assert_eq!(
        invocation
            .to_exec_args(&shell, /*use_login_shell*/ false)
            .last()
            .map(String::as_str),
        Some(script_body)
    );
}

#[test]
fn powershell_script_mode_preserves_exact_nonblank_body() {
    let script_body = "Write-Output foo` ";
    let invocation = parse(
        None,
        Some("powershell_script"),
        None,
        None,
        Some(script_body),
    )
    .expect("nonblank PowerShell script should be accepted without normalization");
    let shell = Shell {
        shell_type: ShellType::PowerShell,
        shell_path: PathBuf::from("pwsh"),
    };

    assert_eq!(
        invocation,
        CommandInvocation::PowerShellScript(script_body.to_string())
    );
    assert_eq!(
        invocation
            .to_safety_args(&shell, /*use_login_shell*/ false)
            .last()
            .map(String::as_str),
        Some(script_body)
    );
}

#[test]
fn powershell_script_mode_builds_encoded_args_without_host_powershell() {
    let script_body = "$value = 'quoted value'; Write-Output $value";
    let invocation = parse(
        None,
        Some("powershell_script"),
        None,
        None,
        Some(script_body),
    )
    .expect("script_body should be accepted");
    let shell = Shell {
        shell_type: ShellType::PowerShell,
        shell_path: PathBuf::from("pwsh"),
    };

    let command = invocation.to_exec_args(&shell, /*use_login_shell*/ false);

    assert_eq!(command.first().map(String::as_str), Some("pwsh"));
    assert!(command.iter().any(|arg| arg == "-NoLogo"));
    assert!(command.iter().any(|arg| arg == "-NoProfile"));
    assert!(command.iter().any(|arg| arg == "-EncodedCommand"));
    assert!(
        !command.iter().any(|arg| arg == script_body),
        "script body should be encoded, not nested as raw shell text"
    );
}

#[test]
fn powershell_script_mode_builds_plain_safety_args_without_host_powershell() {
    let script_body = "Get-ChildItem -Force";
    let invocation = parse(
        None,
        Some("powershell_script"),
        None,
        None,
        Some(script_body),
    )
    .expect("script_body should be accepted");
    let shell = Shell {
        shell_type: ShellType::PowerShell,
        shell_path: PathBuf::from("pwsh"),
    };

    let command = invocation.to_safety_args(&shell, /*use_login_shell*/ false);

    assert_eq!(
        command,
        vec![
            "pwsh".to_string(),
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            script_body.to_string(),
        ]
    );
}

#[test]
fn powershell_script_mode_rejects_mixed_fields() {
    let err = parse(
        Some("Get-ChildItem"),
        Some("powershell_script"),
        None,
        None,
        Some("Get-Process"),
    )
    .expect_err("legacy script field should be rejected");

    assert!(
        err.to_string()
            .contains("received legacy script or argv fields with `kind: \"powershell_script\"`"),
        "unexpected error: {err}"
    );
}

#[test]
fn failure_advisory_only_mentions_powershell_parser_failures() {
    assert!(
        powershell_script_failure_advisory(
            Some(ShellType::PowerShell),
            Some(1),
            false,
            "ParserError: Unexpected token 'foo'",
        )
        .is_some()
    );

    assert_eq!(
        powershell_script_failure_advisory(
            Some(ShellType::PowerShell),
            Some(0),
            false,
            "ParserError: Unexpected token 'foo'",
        ),
        None
    );
    assert_eq!(
        powershell_script_failure_advisory(
            Some(ShellType::Bash),
            Some(1),
            false,
            "ParserError: Unexpected token 'foo'",
        ),
        None
    );
}

#[test]
fn failure_advisory_respects_the_active_powershell_script_mode() {
    assert_eq!(
        powershell_script_failure_advisory(
            Some(ShellType::PowerShell),
            Some(1),
            true,
            "ParserError: Unexpected token 'foo'",
        ),
        None
    );

    let advisory = powershell_script_failure_advisory(
        Some(ShellType::PowerShell),
        Some(1),
        true,
        "Measure-Object : Cannot bind parameter 'Property'. Cannot convert the \"{ $_.Length }\" value of type \"System.Management.Automation.ScriptBlock\" to type \"System.String\".",
    )
    .expect("Measure-Object binding failures should get a targeted hint");

    assert!(advisory.contains("Measure-Object expects property names"));
    assert!(advisory.contains("ForEach-Object"));
}
