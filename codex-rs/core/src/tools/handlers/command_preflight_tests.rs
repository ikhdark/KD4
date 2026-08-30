use super::*;
use pretty_assertions::assert_eq;
use std::path::Path;

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(ToString::to_string).collect()
}

#[test]
fn classifies_repository_wide_and_owner_scoped_rg() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("codex-core is nested under the repository root");
    let scoped = root.join("codex-rs/core/src/tools");

    assert_eq!(
        classify_rg_search_narrowing(&strings(&["rg", "needle", "."]), None, root, root)
            .expect("classification")
            .map(|search| search.breadth),
        Some(RgSearchBreadth::Broad)
    );
    assert_eq!(
        classify_rg_search_narrowing(
            &strings(&["rg", "needle", "codex-rs/core/src/tools"]),
            None,
            root,
            root,
        )
        .expect("classification")
        .map(|search| search.breadth),
        Some(RgSearchBreadth::Narrow)
    );
    assert_eq!(
        classify_rg_search_narrowing(&strings(&["rg", "needle"]), None, &scoped, root)
            .expect("classification")
            .map(|search| search.breadth),
        Some(RgSearchBreadth::Narrow)
    );
    assert_eq!(
        classify_rg_search_narrowing(&strings(&["rg", "--files", "."]), None, root, root)
            .expect("classification")
            .map(|search| search.breadth),
        Some(RgSearchBreadth::Broad)
    );
    assert_eq!(
        classify_rg_search_narrowing(&strings(&["rg", "needle", "codex-rs"]), None, root, root,)
            .expect("classification")
            .map(|search| search.breadth),
        Some(RgSearchBreadth::Broad)
    );
    assert_eq!(
        classify_rg_search_narrowing(&strings(&["rg", "needle", "scripts"]), None, root, root,)
            .expect("classification")
            .map(|search| search.breadth),
        Some(RgSearchBreadth::Narrow)
    );
    let compound = classify_rg_search_narrowing(
        &strings(&[
            "pwsh",
            "-NoProfile",
            "-Command",
            "rg needle . | Measure-Object",
        ]),
        Some(ShellType::PowerShell),
        root,
        root,
    )
    .expect("classification")
    .expect("compound rg should still be gated");
    assert_eq!(compound.breadth, RgSearchBreadth::Broad);
    assert!(!compound.can_record_miss);

    let inventory = classify_rg_search_narrowing(
        &strings(&[
            "powershell.exe",
            "-NoProfile",
            "-Command",
            "$files = rg --files -g 'SOURCEMAP.md' -g 'AGENTS.md' -g '*terminal*' -g '*bench*' -g '*rollout*' -g '*eval*' -g '*prompt*'; Write-Output '---FILES---'; $files; Write-Output '---STATUS---'; git status --short; Write-Output '---ROOT---'; Get-Content -Path AGENTS.md -TotalCount 260; Write-Output '---SOURCEMAP MATCHES---'; rg -n -i 'terminal|benchmark|rollout|prompt|agent loop|tool' SOURCEMAP.md | Select-Object -First 180",
        ]),
        Some(ShellType::PowerShell),
        root,
        root,
    )
    .expect("inventory classification")
    .expect("inventory rg should still be gated");
    assert_eq!(inventory.breadth, RgSearchBreadth::Broad);
    assert!(!inventory.can_record_miss);

    let narrow = classify_rg_search_narrowing(
        &strings(&["rg", "-n", "needle", "codex-rs/core/src"]),
        None,
        root,
        root,
    )
    .expect("classification")
    .expect("narrow rg");
    let broad =
        classify_rg_search_narrowing(&strings(&["rg", "-n", "needle", "."]), None, root, root)
            .expect("classification")
            .expect("broad rg");
    assert_eq!(narrow.query_identity, broad.query_identity);
    let equivalent_narrow = classify_rg_search_narrowing(
        &strings(&["rg", "-n", "needle", "./codex-rs/core/src"]),
        None,
        root,
        root,
    )
    .expect("classification")
    .expect("equivalent narrow rg");
    assert_eq!(narrow.search_identity, equivalent_narrow.search_identity);
    assert_ne!(narrow.search_identity, broad.search_identity);

    let reordered_targets = classify_rg_search_narrowing(
        &strings(&[
            "rg",
            "-n",
            "needle",
            "codex-rs/core/src/session",
            "codex-rs/core/src/tools",
        ]),
        None,
        root,
        root,
    )
    .expect("classification")
    .expect("multi-target rg");
    let equivalent_targets = classify_rg_search_narrowing(
        &strings(&[
            "rg",
            "-n",
            "needle",
            "./codex-rs/core/src/tools",
            "codex-rs/core/src/missing/../session",
            "codex-rs/core/src/tools",
        ]),
        None,
        root,
        root,
    )
    .expect("classification")
    .expect("equivalent multi-target rg");
    assert_eq!(
        reordered_targets.search_identity,
        equivalent_targets.search_identity
    );

    let first_pattern = classify_rg_search_narrowing(
        &strings(&["rg", "--files-with-matches", "first", "codex-rs/core/src"]),
        None,
        root,
        root,
    )
    .expect("classification")
    .expect("first files-with-matches query");
    let second_pattern = classify_rg_search_narrowing(
        &strings(&["rg", "--files-with-matches", "second", "codex-rs/core/src"]),
        None,
        root,
        root,
    )
    .expect("classification")
    .expect("second files-with-matches query");
    assert_ne!(first_pattern.query_identity, second_pattern.query_identity);
}

#[test]
fn classifies_cross_owner_and_outside_repository_targets_as_broad() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("codex-core is nested under the repository root");

    for command in [
        strings(&["rg", "needle", ".codex", "scripts", "docs", "packages"]),
        strings(&["rg", "needle", "codex-rs/core", "codex-rs/protocol"]),
        strings(&["rg", "needle", ".."]),
    ] {
        let search = classify_rg_search_narrowing(&command, None, root, root)
            .expect("classification")
            .expect("rg search");
        assert_eq!(search.breadth, RgSearchBreadth::Broad);
    }
}

#[test]
fn records_only_the_immediate_parent_as_the_next_expansion_scope() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("codex-core is nested under the repository root");
    let narrow = classify_rg_search_narrowing(
        &strings(&["rg", "needle", "codex-rs/core/src/tools"]),
        None,
        root,
        root,
    )
    .expect("classification")
    .expect("narrow rg");
    let parent = classify_rg_search_narrowing(
        &strings(&["rg", "needle", "codex-rs/core/src"]),
        None,
        root,
        root,
    )
    .expect("classification")
    .expect("parent rg");
    let repository =
        classify_rg_search_narrowing(&strings(&["rg", "needle", "."]), None, root, root)
            .expect("classification")
            .expect("repository rg");

    assert_eq!(
        narrow.parent_scope_identity.as_deref(),
        Some(parent.scope_identity.as_str())
    );
    assert_ne!(
        narrow.parent_scope_identity.as_deref(),
        Some(repository.scope_identity.as_str())
    );
}

#[test]
fn search_narrowing_is_best_effort_for_dynamic_and_non_native_commands() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("codex-core is nested under the repository root");
    let dynamic_powershell = strings(&[
        "pwsh",
        "-NoProfile",
        "-Command",
        "$tool = 'rg'; & $tool needle .",
    ]);

    assert_eq!(
        classify_rg_search_narrowing(&dynamic_powershell, Some(ShellType::PowerShell), root, root,),
        Ok(None)
    );
    assert_eq!(
        classify_rg_search_narrowing_without_native_scope(&strings(&["rg", "needle", "src"]), None,),
        None
    );
    assert_eq!(
        classify_rg_search_narrowing_without_native_scope(
            &strings(&["git", "status", "--short"]),
            None,
        ),
        None
    );
}

#[test]
fn ignores_rg_metadata_modes_and_option_values_as_search_paths() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("codex-core is nested under the repository root");

    for command in [
        ["rg", "--help"].as_slice(),
        ["rg", "--version"].as_slice(),
        ["rg", "--type-list"].as_slice(),
        ["rg", "--pcre2-version"].as_slice(),
        ["rg", "--generate=complete-powershell"].as_slice(),
    ] {
        assert_eq!(
            classify_rg_search_narrowing(&strings(command), None, root, root),
            Ok(None)
        );
    }

    let search = classify_rg_search_narrowing(
        &strings(&[
            "rg",
            "--max-depth",
            "3",
            "--type-add",
            "source:*.rs",
            "needle",
            "codex-rs/core/src/tools",
        ]),
        None,
        root,
        root,
    )
    .expect("classification")
    .expect("rg search with option value");
    assert_eq!(search.breadth, RgSearchBreadth::Narrow);
    assert!(search.query_identity.contains("source:*.rs"));
}

#[test]
fn confirmed_performance_non_rg_commands_skip_search_path_normalization() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    crate::tools::handlers::command_search::reset_search_path_normalization_count();

    assert_eq!(
        classify_rg_search_narrowing(&strings(&["git", "status", "--short"]), None, root, root,),
        Ok(None)
    );
    assert_eq!(
        crate::tools::handlers::command_search::search_path_normalization_count(),
        0
    );

    let repository_root = root
        .parent()
        .and_then(Path::parent)
        .expect("codex-core is nested under the repository root");
    crate::tools::handlers::command_search::reset_search_path_normalization_count();
    classify_rg_search_narrowing(
        &strings(&["rg", "needle", "codex-rs/core/src/tools"]),
        None,
        repository_root,
        repository_root,
    )
    .expect("search classification")
    .expect("rg search");
    assert_eq!(
        crate::tools::handlers::command_search::search_path_normalization_count(),
        2,
        "the repository root and target should each be normalized once"
    );
}

#[tokio::test]
async fn search_scope_state_ignores_unrelated_content_and_detects_target_changes() {
    let temp = tempfile::tempdir().expect("search scope fixture");
    let root = temp.path();
    let src = root.join("src");
    std::fs::create_dir(&src).expect("create target scope");
    std::fs::write(src.join("lib.rs"), b"original").expect("write target file");
    std::fs::write(root.join("outside.bin"), vec![b'x'; 1024 * 1024])
        .expect("write unrelated content");
    let command = strings(&["rg", "needle", "src"]);

    let mut first = classify_rg_search_narrowing(&command, None, root, root)
        .expect("classification")
        .expect("rg search");
    crate::tools::handlers::command_search::observe_rg_search_scope_state(&mut first).await;
    let first_identity = first
        .scope_state_identity
        .expect("target scope exposes a native filesystem identity");

    std::fs::write(root.join("outside.bin"), vec![b'y'; 2 * 1024 * 1024])
        .expect("change unrelated content");
    let mut after_unrelated = classify_rg_search_narrowing(&command, None, root, root)
        .expect("classification")
        .expect("rg search");
    crate::tools::handlers::command_search::observe_rg_search_scope_state(&mut after_unrelated)
        .await;
    assert_eq!(
        after_unrelated.scope_state_identity.as_deref(),
        Some(first_identity.as_str()),
        "content outside the rg targets must not invalidate the scoped miss"
    );

    std::fs::write(src.join("lib.rs"), b"changed!").expect("change target content");
    let mut after_relevant = classify_rg_search_narrowing(&command, None, root, root)
        .expect("classification")
        .expect("rg search");
    crate::tools::handlers::command_search::observe_rg_search_scope_state(&mut after_relevant)
        .await;
    assert_ne!(
        after_relevant.scope_state_identity.as_deref(),
        Some(first_identity.as_str()),
        "a target change must invalidate the scoped miss"
    );
}

#[test]
fn repairs_direct_argv_git_status_to_disable_optional_locks() {
    let invocation = CommandInvocation::Argv {
        program: "git".to_string(),
        args: strings(&["status", "--short", "--branch"]),
    };

    let outcome = preflight_invocation_with_equivalent_repair(
        &invocation,
        &invocation.to_direct_argv().expect("argv"),
        None,
    )
    .expect("git status should receive a read-only equivalent repair");

    assert_eq!(
        outcome.invocation,
        CommandInvocation::Argv {
            program: "git".to_string(),
            args: strings(&["--no-optional-locks", "status", "--short", "--branch"]),
        }
    );
    assert!(
        outcome
            .repair_notice
            .as_deref()
            .is_some_and(|notice| notice.contains("git_status_optional_locks"))
    );
}

#[test]
fn does_not_rewrite_git_status_scripts_or_unrelated_git_commands() {
    let script = CommandInvocation::Script("git status".to_string());
    let script_outcome = preflight_invocation_with_equivalent_repair(
        &script,
        &strings(&["bash", "-lc", "git status"]),
        Some(ShellType::Bash),
    )
    .expect("git status script remains valid but potentially mutating");
    assert_eq!(script_outcome.invocation, script);
    assert!(!script_outcome.repaired());

    let branch = CommandInvocation::Argv {
        program: "git".to_string(),
        args: strings(&["branch", "new-branch"]),
    };
    let branch_outcome = preflight_invocation_with_equivalent_repair(
        &branch,
        &branch.to_direct_argv().expect("argv"),
        None,
    )
    .expect("git branch remains valid but potentially mutating");
    assert_eq!(branch_outcome.invocation, branch);
    assert!(!branch_outcome.repaired());
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
fn command_preflight_accepts_dynamic_shell_command_when_static_parse_is_inconclusive() {
    let commands = preflight_command_issue(
        &strings(&[
            "pwsh",
            "-NoProfile",
            "-Command",
            "$tool = 'git'; & $tool status --short",
        ]),
        /*shell_type*/ None,
    )
    .expect("static parser uncertainty must not reject a valid dynamic command");

    assert!(commands.is_empty());
}

#[test]
fn direct_argv_accepts_executable_with_powershell_cmdlet_shape() {
    let commands = preflight_command_issue(
        &strings(&["Get-Widget", "--version"]),
        /*shell_type*/ None,
    )
    .expect("an unknown Verb-Noun executable is valid direct argv");

    assert!(commands.is_empty());
}

#[test]
fn repairs_one_read_only_direct_argv_typo() {
    let invocation = CommandInvocation::Argv {
        program: "rg".to_string(),
        args: strings(&["--ignorecase", "TODO", "src"]),
    };
    let outcome = preflight_invocation_with_equivalent_repair(
        &invocation,
        &invocation.to_direct_argv().expect("argv"),
        None,
    )
    .expect("read-only typo should be repaired");

    assert_eq!(
        outcome.invocation,
        CommandInvocation::Argv {
            program: "rg".to_string(),
            args: strings(&["--ignore-case", "TODO", "src"]),
        }
    );
    assert!(outcome.repaired());
    assert!(
        outcome
            .repair_notice
            .as_deref()
            .is_some_and(|notice| notice.contains("read-only equivalent repair"))
    );
}

#[test]
fn arguments_after_double_dash_are_not_linted_or_repaired() {
    let invocation = CommandInvocation::Argv {
        program: "rg".to_string(),
        args: strings(&["TODO", "src", "--", "--ignorecase"]),
    };
    let outcome = preflight_invocation_with_equivalent_repair(
        &invocation,
        &invocation.to_direct_argv().expect("argv"),
        None,
    )
    .expect("arguments after -- belong to the invoked program");

    assert_eq!(outcome.invocation, invocation);
    assert!(!outcome.repaired());

    let invocation = CommandInvocation::Argv {
        program: "rg".to_string(),
        args: strings(&["literal", "--", "-gdata\\with\\backslashes"]),
    };
    let outcome = preflight_invocation_with_equivalent_repair(
        &invocation,
        &invocation.to_direct_argv().expect("argv"),
        None,
    )
    .expect("glob-like data after -- must remain literal data");

    assert_eq!(outcome.invocation, invocation);
    assert!(!outcome.repaired());
}

#[test]
fn never_repairs_mutating_command_flag_typos() {
    let invocation = CommandInvocation::Argv {
        program: "git".to_string(),
        args: strings(&["--worktree", "status"]),
    };
    let issue = preflight_invocation_with_equivalent_repair_detailed(
        &invocation,
        &invocation.to_direct_argv().expect("argv"),
        None,
    )
    .expect_err("mutating-capable tools must never be repaired automatically");

    assert_eq!(issue.code, CommandPreflightIssueCode::KnownFlagTypo);
    assert_eq!(
        issue.retry,
        Some(CommandPreflightRetry::Argv {
            program: "git".to_string(),
            args: strings(&["--work-tree", "status"]),
        })
    );
}

#[test]
fn never_repairs_script_even_when_first_command_is_read_only() {
    let invocation =
        CommandInvocation::Script("rg --ignorecase TODO .; Remove-Item -Recurse build".to_string());
    let command = strings(&[
        "pwsh",
        "-NoProfile",
        "-Command",
        "rg --ignorecase TODO .; Remove-Item -Recurse build",
    ]);

    let issue = preflight_invocation_with_equivalent_repair_detailed(
        &invocation,
        &command,
        Some(ShellType::PowerShell),
    )
    .expect_err("scripts must be rejection-only");
    assert_eq!(issue.code, CommandPreflightIssueCode::KnownFlagTypo);
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
fn files_with_matches_keeps_the_first_positional_argument_as_a_pattern() {
    preflight_command(
        &strings(&[
            "rg",
            "--files-with-matches",
            "TODO|*.rs",
            "codex-rs/core/src",
        ]),
        None,
    )
    .expect("files-with-matches still takes a search pattern before its paths");
}

#[test]
fn files_mode_still_treats_each_positional_argument_as_a_path() {
    let issue = preflight_command_issue(&strings(&["rg", "--files", "codex-rs/*/src"]), None)
        .expect_err("--files has path operands and direct argv does not expand them");

    assert_eq!(issue.code, CommandPreflightIssueCode::RgLiteralGlobPath);
}

#[test]
fn supported_powershell_launcher_shapes_are_not_rejected_by_partial_static_parsing() {
    for invocation in [
        strings(&[
            "pwsh",
            "-NonInteractive",
            "-Command",
            "Write-Output ok",
            "extra",
        ]),
        strings(&[
            "powershell.exe",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            "script.ps1",
            "argument",
        ]),
        strings(&[
            "pwsh",
            "-EncodedCommand",
            "VwByAGkAdABlAC0ATwB1AHQAcAB1AHQA",
        ]),
    ] {
        preflight_command(&invocation, None)
            .unwrap_or_else(|issue| panic!("supported launcher shape was rejected: {issue:?}"));
    }
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
fn powershell_shell_mismatch_help_is_windows_only() {
    let issue = preflight_command_issue(
        &strings(&["pwsh", "-NoProfile", "-Command", "export CODEX_ENV=windows"]),
        Some(ShellType::PowerShell),
    )
    .expect_err("POSIX syntax in PowerShell should be rejected");

    let rendered = issue.render_for_model();
    assert!(rendered.contains("rewrite the command for PowerShell"));
    assert!(!rendered.contains("select a POSIX shell"), "{rendered}");
}

#[test]
fn powershell_shell_mismatch_ignores_quoted_text_and_comments() {
    preflight_command(
        &strings(&[
            "pwsh",
            "-NoProfile",
            "-Command",
            "Write-Output 'export PATH'; Write-Output \"source ./env 2>/dev/null\"; # export OTHER=value\nWrite-Output done",
        ]),
        Some(ShellType::PowerShell),
    )
    .expect("quoted data and comments are not active POSIX syntax");
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

#[tokio::test]
async fn direct_runtime_does_not_gate_execution_on_preflight_heuristics() {
    let script = "Write-Output 'unterminated";
    let invocation = CommandInvocation::PowerShellScript(script.to_string());
    let command = strings(&["pwsh", "-NoProfile", "-Command", script]);
    preflight_invocation_with_equivalent_repair(&invocation, &command, Some(ShellType::PowerShell))
        .expect_err("legacy preflight should reject the fixture");

    let outcome = preflight_invocation_for_runtime(
        /*direct_runtime*/ true,
        &invocation,
        &command,
        Some(ShellType::PowerShell),
    )
    .await
    .expect("direct runtime should preserve the authoritative command");

    assert_eq!(outcome.invocation, invocation);
    assert!(outcome.validation_invocations.is_empty());
    assert_eq!(outcome.repair_notice, None);
}

#[test]
fn accepts_powershell_backslash_before_closing_quote() {
    preflight_command(
        &strings(&[
            "pwsh",
            "-NoProfile",
            "-Command",
            r#"Write-Output "C:\foo\""#,
        ]),
        Some(ShellType::PowerShell),
    )
    .expect("PowerShell uses backticks rather than backslashes as quote escapes");
}

#[test]
fn accepts_powershell_backtick_escaped_quote_and_comment_apostrophe() {
    preflight_command(
        &strings(&[
            "pwsh",
            "-NoProfile",
            "-Command",
            "Write-Output \"a`\"b\" # user's text",
        ]),
        Some(ShellType::PowerShell),
    )
    .expect("PowerShell quote escapes and comment text should be parsed with PowerShell rules");
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
fn literal_path_lint_windows_only_help_matches_path_parameter_colon_form() {
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
    let rendered = issue.render_for_model();
    assert!(rendered.contains("-LiteralPath"));
    assert!(rendered.contains("cmd quoting example"));
    assert!(!rendered.contains("POSIX"), "{rendered}");
}

#[test]
fn literal_path_lint_accepts_plain_path_parameter() {
    lint_windows_path_shape(
        r"Get-Content -Path C:\repo\plain.txt",
        Some(ShellType::PowerShell),
        &[strings(&["Get-Content", "-Path", r"C:\repo\plain.txt"])],
    )
    .expect("plain -Path values do not need literal-path rewriting");
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

#[test]
fn preflight_preserves_each_plain_pipeline_stage_for_validation() {
    let script = "cargo test | Select-Object -First 1";
    let invocation = CommandInvocation::PowerShellScript(script.to_string());
    let outcome = preflight_invocation_with_equivalent_repair(
        &invocation,
        &strings(&["pwsh", "-NoProfile", "-Command", script]),
        Some(ShellType::PowerShell),
    )
    .expect("the deterministic pipeline should pass preflight");

    assert_eq!(
        outcome.validation_invocations,
        vec![
            CommandInvocation::Argv {
                program: "cargo".to_string(),
                args: vec!["test".to_string()],
            },
            CommandInvocation::Argv {
                program: "Select-Object".to_string(),
                args: vec!["-First".to_string(), "1".to_string()],
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_command_analysis_does_not_stall_the_async_runtime() {
    let analysis = crate::tools::run_blocking_command_analysis(|| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        42
    });
    tokio::pin!(analysis);

    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        result = &mut analysis => panic!("blocking analysis completed on the runtime thread: {result:?}"),
    }

    assert_eq!(analysis.await.expect("blocking worker"), 42);
}
