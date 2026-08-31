use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::json;

use crate::FunctionCallError;
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
fn authorization_identity_preserves_direct_argv_program() {
    let args = vec!["--version".to_string()];
    let invocation = parse(None, Some("argv"), Some(" tool.exe "), Some(&args), None)
        .expect("nonblank program should parse without normalization");
    let shell = Shell {
        shell_type: ShellType::PowerShell,
        shell_path: PathBuf::from("pwsh"),
    };

    assert_eq!(
        invocation,
        CommandInvocation::Argv {
            program: " tool.exe ".to_string(),
            args,
        }
    );
    assert_eq!(
        invocation
            .to_exec_args(&shell, /*use_login_shell*/ false)
            .expect("direct argv"),
        vec![" tool.exe ".to_string(), "--version".to_string()]
    );
}

#[test]
fn structured_native_commands_select_direct_argv() {
    let cases: [(&str, &[&str]); 6] = [
        ("git", &["status", "--short"]),
        ("rg", &["--files"]),
        ("cargo", &["test", "-p", "codex-core"]),
        ("node", &["script.js", "--flag"]),
        ("python", &["-m", "pytest"]),
        ("kds", &["--help"]),
    ];

    for (program, args) in cases {
        let args = args
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>();
        let invocation = parse(None, Some("argv"), Some(program), Some(&args), None)
            .expect("structured native command should parse as argv");
        let mut expected_argv = vec![program.to_string()];
        expected_argv.extend(args.clone());

        assert_eq!(
            invocation,
            CommandInvocation::Argv {
                program: program.to_string(),
                args,
            },
            "{program} should retain the producer-selected argv representation"
        );
        assert_eq!(invocation.to_direct_argv(), Some(expected_argv));
    }
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
    let rewritten_args = vec![
        "--agent".to_string(),
        "path with spaces".to_string(),
        "quote\"inside".to_string(),
        String::new(),
        "Grüße 世界".to_string(),
    ];
    let expected = CommandInvocation::Argv {
        program: "kds".to_string(),
        args: rewritten_args.clone(),
    };
    let display_command = expected.display_command();

    let updated = invocation
        .with_updated_hook_input(
            "exec_command",
            &json!({
                "command": display_command,
                "kind": "argv",
                "program": "kds",
                "args": rewritten_args,
            }),
        )
        .expect("structured argv rewrite should be accepted");

    assert_eq!(updated, expected);
    assert_eq!(
        updated.to_direct_argv(),
        Some(vec![
            "kds".to_string(),
            "--agent".to_string(),
            "path with spaces".to_string(),
            "quote\"inside".to_string(),
            String::new(),
            "Grüße 世界".to_string(),
        ])
    );
}

#[test]
fn argv_hook_rewrite_rejects_mixed_or_misleading_shapes() {
    let invocation = parse(None, Some("argv"), Some("rg"), None, None).expect("argv should parse");

    let inspected = invocation
        .with_updated_hook_input(
            "exec_command",
            &json!({ "command": invocation.display_command() }),
        )
        .expect("an unchanged display-only response should preserve argv");
    assert_eq!(inspected, invocation);
    assert_eq!(inspected.to_direct_argv(), Some(vec!["rg".to_string()]));

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
fn tagged_script_is_canonical_and_tagged_legacy_is_rejected() {
    let script =
        parse(Some("Write-Output ok"), Some("script"), None, None, None).expect("tagged script");
    let legacy_error = parse(Some("Write-Output ok"), Some("legacy"), None, None, None)
        .expect_err("legacy input must omit kind so the decoder can normalize it");

    assert_eq!(
        script,
        CommandInvocation::Script("Write-Output ok".to_string())
    );
    let message = legacy_error.to_string();
    assert!(message.contains("$.kind"), "{message}");
    assert!(message.contains("actual value `legacy`"), "{message}");
    assert!(message.contains("omit `kind`"), "{message}");
}

#[test]
fn shell_semantics_remain_scripts_when_the_producer_selects_script_mode() {
    let shell_commands = [
        "Get-ChildItem -Force",
        "$env:FOO = 'bar'",
        "rg --files | Select-String rs",
        "rg --files > files.txt",
        "git status; rg --files",
        "dir",
        "build.cmd /quiet",
        "build.bat /quiet",
    ];

    for command in shell_commands {
        let invocation = parse(Some(command), Some("script"), None, None, None)
            .expect("producer-selected script should remain a script");

        assert_eq!(
            invocation,
            CommandInvocation::Script(command.to_string()),
            "script mode must preserve the KD4 shell contract for {command}"
        );
        assert_eq!(invocation.to_direct_argv(), None);
    }
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
            .expect("PowerShell args")
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
            .expect("PowerShell safety args")
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

    let command = invocation
        .to_exec_args(&shell, /*use_login_shell*/ false)
        .expect("PowerShell args");

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

    let command = invocation
        .to_safety_args(&shell, /*use_login_shell*/ false)
        .expect("PowerShell safety args");

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
fn powershell_exec_and_safety_projections_encode_the_same_script() {
    let script_body = "$value = 'quoted value'; Write-Output $value";
    let invocation = CommandInvocation::PowerShellScript(script_body.to_string());
    let shell = Shell {
        shell_type: ShellType::PowerShell,
        shell_path: PathBuf::from("pwsh"),
    };
    let exec = invocation
        .to_exec_args(&shell, /*use_login_shell*/ false)
        .expect("PowerShell execution args");
    let safety = invocation
        .to_safety_args(&shell, /*use_login_shell*/ false)
        .expect("PowerShell safety args");
    let exec_mode = exec
        .iter()
        .position(|arg| arg == "-EncodedCommand")
        .expect("encoded execution mode");
    let safety_mode = safety
        .iter()
        .position(|arg| arg == "-Command")
        .expect("plain safety mode");

    assert_eq!(&exec[..exec_mode], &safety[..safety_mode]);
    assert_eq!(
        safety.get(safety_mode + 1).map(String::as_str),
        Some(script_body)
    );

    let encoded = exec.get(exec_mode + 1).expect("encoded script payload");
    let bytes = BASE64_STANDARD.decode(encoded).expect("base64 payload");
    let utf16 = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    assert_eq!(
        String::from_utf16(&utf16).expect("UTF-16LE payload"),
        format!(
            "{}{}",
            codex_shell_command::powershell::UTF8_OUTPUT_PREFIX,
            script_body
        )
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

    let message = err.to_string();
    assert!(message.contains("`powershell_script` branch"), "{message}");
    assert!(
        message.contains("$.command") || message.contains("$.cmd"),
        "{message}"
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
