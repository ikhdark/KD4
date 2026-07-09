use super::*;
use pretty_assertions::assert_eq;
use std::path::Path;

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(ToString::to_string).collect()
}

#[test]
fn rejects_known_rg_flag_typo_for_direct_argv() {
    let issue = preflight_command_issue(
        &strings(&["rg", "--ignorecase", "TODO", "src"]),
        /*shell_type*/ None,
    )
    .expect_err("typo should be rejected");

    assert_eq!(issue.code, CommandPreflightIssueCode::KnownFlagTypo);
    let rendered = issue.render_for_model();
    assert!(rendered.contains("`rg` has no `--ignorecase` flag"));
    assert!(rendered.contains("kind: \"argv\""));
    assert!(rendered.contains("\"--ignore-case\""));
    assert!(rendered.contains("\"kind\":\"known_flag_typo\""));
}

#[test]
fn rejects_known_flag_typos_case_insensitively() {
    let issue = preflight_command_issue(
        &strings(&["RG", "--IGNORECASE", "TODO", "src"]),
        /*shell_type*/ None,
    )
    .expect_err("executable and flag casing should not hide known typos");

    assert_eq!(issue.code, CommandPreflightIssueCode::KnownFlagTypo);
    assert_eq!(
        issue.retry,
        Some(CommandPreflightRetry::Argv {
            program: "RG".to_string(),
            args: vec![
                "--ignore-case".to_string(),
                "TODO".to_string(),
                "src".to_string()
            ],
        })
    );
}

#[test]
fn rejects_rg_glob_backslashes_for_direct_argv() {
    let issue = preflight_command_issue(
        &strings(&["rg", "--files", "--glob", r"core\**\*.rs"]),
        /*shell_type*/ None,
    )
    .expect_err("rg glob patterns should use slash separators");

    assert_eq!(issue.code, CommandPreflightIssueCode::RgGlobPathSeparator);
    let rendered = issue.render_for_model();
    assert!(rendered.contains("gitignore-style `/` separators"));
    assert!(rendered.contains("kind: \"argv\""));
    assert!(rendered.contains("\"core/**/*.rs\""));
}

#[test]
fn rejects_rg_literal_glob_path_for_direct_argv() {
    let issue = preflight_command_issue(
        &strings(&["rg", "-n", "TODO", ".codex/skills/*/SKILL.md"]),
        /*shell_type*/ None,
    )
    .expect_err("direct argv should not pass unexpanded glob-looking paths to rg");

    assert_eq!(issue.code, CommandPreflightIssueCode::RgLiteralGlobPath);
    let rendered = issue.render_for_model();
    assert!(rendered.contains("not shell-expanded"));
    assert!(rendered.contains("pass wildcards through `--glob`"));
    assert!(rendered.contains("\"kind\":\"rg_literal_glob_path\""));
}

#[test]
fn accepts_rg_literal_glob_path_in_posix_script() {
    preflight_command(
        &strings(&["/bin/bash", "-lc", "rg -n TODO .codex/skills/*/SKILL.md"]),
        Some(ShellType::Bash),
    )
    .expect("POSIX shells expand glob-looking path operands before rg receives them");
}

#[test]
fn rejects_powershell_cmdlets_for_direct_argv() {
    let issue = preflight_command_issue(
        &strings(&["get-content", "-LiteralPath", r"C:\repo\file.txt"]),
        /*shell_type*/ None,
    )
    .expect_err("PowerShell cmdlets are not direct executables");

    assert_eq!(
        issue.code,
        CommandPreflightIssueCode::DirectArgvPowerShellCmdlet
    );
    let rendered = issue.render_for_model();
    assert!(rendered.contains("not a standalone executable"));
    assert!(rendered.contains("kind: \"powershell_script\""));
    assert!(rendered.contains("\"kind\":\"direct_argv_powershell_cmdlet\""));
}

#[test]
fn powershell_cmdlet_retry_uses_powershell_literal_quoting() {
    let issue = preflight_command_issue(
        &strings(&[
            "Get-Content",
            "-LiteralPath",
            r"C:\repo\path with spaces\it's.txt",
        ]),
        /*shell_type*/ None,
    )
    .expect_err("PowerShell cmdlets are not direct executables");

    assert_eq!(
        issue.retry,
        Some(CommandPreflightRetry::PowerShellScript {
            script_body: r"Get-Content -LiteralPath 'C:\repo\path with spaces\it''s.txt'"
                .to_string(),
        })
    );
}

#[test]
fn rejects_powershell_measure_object_scriptblock_property() {
    let issue = preflight_command_issue(
        &strings(&[
            "pwsh",
            "-NoProfile",
            "-Command",
            "Get-ChildItem | Measure-Object -Property { $_.Length } -Sum",
        ]),
        Some(ShellType::PowerShell),
    )
    .expect_err("Measure-Object -Property script blocks should be rejected");

    assert_eq!(
        issue.code,
        CommandPreflightIssueCode::PowerShellMeasureObjectScriptBlockProperty
    );
    let rendered = issue.render_for_model();
    assert!(rendered.contains("expects property names"));
    assert!(rendered.contains("ForEach-Object"));
}

#[test]
fn accepts_measure_object_property_names_in_powershell_script() {
    preflight_command(
        &strings(&[
            "pwsh",
            "-NoProfile",
            "-Command",
            "Get-ChildItem | Measure-Object -Property Length -Sum",
        ]),
        Some(ShellType::PowerShell),
    )
    .expect("Measure-Object property names should remain valid");
}

#[test]
fn rejects_powershell_shape_in_posix_script() {
    let issue = preflight_command_issue(
        &strings(&["/bin/bash", "-lc", "Get-ChildItem -Force"]),
        Some(ShellType::Bash),
    )
    .expect_err("PowerShell cmdlet in POSIX shell should be rejected");

    assert_eq!(issue.code, CommandPreflightIssueCode::ShellMismatch);
    assert!(issue.render_for_model().contains("PowerShell syntax"));
}

#[test]
fn rejects_unbalanced_quotes_in_shell_script() {
    let issue = preflight_command_issue(
        &strings(&["/bin/bash", "-lc", "rg 'TODO src"]),
        Some(ShellType::Bash),
    )
    .expect_err("unbalanced quotes should be rejected");

    assert_eq!(issue.code, CommandPreflightIssueCode::UnbalancedQuotes);
    assert!(
        issue
            .render_for_model()
            .contains("missing closing single quote")
    );
}

#[test]
fn accepts_posix_heredoc_body_with_apostrophe() {
    preflight_command(
        &strings(&[
            "/bin/bash",
            "-lc",
            "apply_patch <<'PATCH'\n*** Begin Patch\n*** Add File: note.txt\n+it's fine\n*** End Patch\nPATCH",
        ]),
        Some(ShellType::Bash),
    )
    .expect("quoted here-doc bodies should not be scanned as shell syntax");
}

#[test]
fn rejects_powershell_cmdlets_under_cmd() {
    let err = preflight_command(
        &strings(&["cmd.exe", "/d", "/s", "/c", "Get-Content file.txt"]),
        /*shell_type*/ None,
    )
    .expect_err("cmd.exe scripts should reject PowerShell cmdlets");

    assert!(err.contains("PowerShell cmdlet"));
}

#[test]
fn literal_path_lint_matches_path_parameter_colon_form() {
    let issue = lint_windows_path_shape(
        r"Get-ChildItem -Path:C:\repo\[name]",
        Some(ShellType::PowerShell),
        &[strings(&["Get-ChildItem", r"-Path:C:\repo\[name]"])],
    )
    .expect_err("PowerShell -Path: parameters should be recognized");

    assert_eq!(
        issue.code,
        CommandPreflightIssueCode::WindowsLiteralPathRequired
    );
    assert!(issue.render_for_model().contains("-LiteralPath"));
}

#[test]
fn literal_path_lint_accepts_literal_path_case_insensitively() {
    lint_windows_path_shape(
        r"Get-ChildItem -literalpath C:\repo\[name]",
        Some(ShellType::PowerShell),
        &[strings(&[
            "Get-ChildItem",
            "-literalpath",
            r"C:\repo\[name]",
        ])],
    )
    .expect("PowerShell -LiteralPath parameters are case-insensitive");
}

#[test]
fn renders_shell_path_literals() {
    let path = Path::new(r"C:\A B\[x]\it's.txt");
    assert_eq!(
        powershell_literal_path_arg(path),
        vec![
            "-LiteralPath".to_string(),
            r#"'C:\A B\[x]\it''s.txt'"#.to_string()
        ]
    );
    assert_eq!(cmd_quoted_path(path), r#""C:\A B\[x]\it's.txt""#);
    assert_eq!(posix_single_quoted(path), r#"'C:\A B\[x]\it'"'"'s.txt'"#);
}

#[test]
fn render_truncates_rejected_command_on_char_boundary() {
    let mut long_non_ascii = "é".repeat(130);
    long_non_ascii.push_str("--ignorecase");
    let issue = CommandPreflightIssue::reject(
        CommandPreflightIssueCode::KnownFlagTypo,
        CommandPreflightRejected::Script(long_non_ascii),
        "test detail".to_string(),
        None,
        None,
    );

    let rendered = issue.render_for_model();

    assert!(rendered.contains("..."));
    assert!(rendered.contains("test detail"));
}
