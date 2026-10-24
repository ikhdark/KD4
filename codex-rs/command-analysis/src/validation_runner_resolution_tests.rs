use super::*;

// Runner recognition is classification, not proof of execution. The lean
// command result no longer carries the removed runner-receipt fields.
fn has_authoritative_classification(validation: &CommandValidation) -> bool {
    matches!(validation.classification, ValidationClassification::Validation {
        has_unclassified_targets: false, exit_code_is_authoritative: true, ..
    })
}

#[tokio::test]
async fn committed_just_wait_override_reaches_the_trusted_resolver() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let git = |args: &[&str]| {
        let output = std::process::Command::new(codex_git_utils::git_executable())
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init"]);
    std::fs::create_dir(root.join(".codex")).unwrap();
    std::fs::write(
        root.join(".codex/test-runners.json"),
        include_str!("../../../.codex/test-runners.json"),
    )
    .unwrap();
    git(&["add", ".codex/test-runners.json"]);
    git(&["commit", "-m", "trusted runner fixture"]);

    for script in [
        "just core-gate capability-command-preflight",
        "just --set rust_validation_wait_seconds 5 core-gate capability-command-preflight",
    ] {
        for invocation in [
            CommandInvocation::Script(script.into()),
            CommandInvocation::Argv {
                program: "just".into(),
                args: script
                    .split_whitespace()
                    .skip(1)
                    .map(str::to_owned)
                    .collect(),
            },
        ] {
            let validation =
                resolve_command_validation_with_shell(&invocation, None, Some(root), None)
                    .await
                    .unwrap();
            assert!(validation.is_test());
            assert!(validation.signal().get("proof").is_none());
            assert!(has_authoritative_classification(&validation), "{script}");
        }
        #[cfg(windows)]
        {
            let validation = resolve_command_validation_with_shell(
                &CommandInvocation::Script(script.into()),
                Some(codex_shell_command::shell_detect::ShellType::PowerShell),
                Some(root),
                None,
            )
            .await
            .unwrap();
            assert!(has_authoritative_classification(&validation), "PowerShell: {script}");
        }
    }
    for script in [
        "just --set shell evil core-gate capability-command-preflight",
        "just --set rust_validation_wait_seconds NaN core-gate capability-command-preflight",
        "just --set rust_validation_wait_seconds 5 core-gate capability-command-preflight --justfile foreign.just",
        "just --set rust_validation_wait_seconds 5 core-gate capability-command-preflight --dry-run",
        "just --set rust_validation_wait_seconds 5 core-gate capability-command-preflight; echo forged",
    ] {
        let validation = resolve_command_validation_with_shell(
            &CommandInvocation::Script(script.into()),
            None,
            Some(root),
            None,
        )
        .await;
        assert!(
            validation.is_none_or(|validation| !has_authoritative_classification(&validation)),
            "{script}"
        );
        #[cfg(windows)]
        {
            let validation = resolve_command_validation_with_shell(
                &CommandInvocation::Script(script.into()),
                Some(codex_shell_command::shell_detect::ShellType::PowerShell),
                Some(root),
                None,
            )
            .await;
            assert!(
                validation.is_none_or(|validation| !has_authoritative_classification(&validation)),
                "PowerShell: {script}"
            );
        }
    }
    // Dirty declarations cannot broaden the committed trust boundary.
    std::fs::write(root.join(".codex/test-runners.json"), "{}").unwrap();
    let validation = resolve_command_validation_with_shell(
        &CommandInvocation::Script(
            "just --set rust_validation_wait_seconds 5 core-gate capability-command-preflight"
                .into(),
        ),
        None,
        Some(root),
        None,
    )
    .await
    .unwrap();
    assert!(has_authoritative_classification(&validation));
}
