use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn evaluates_powershell_inner_commands_against_prompt_rules() {
    assert_exec_approval_requirement_for_command(
        ExecApprovalRequirementScenario {
            policy_src: Some(r#"prefix_rule(pattern=["echo"], decision="prompt")"#.to_string()),
            command: vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "echo blocked".to_string(),
            ],
            approval_policy: AskForApproval::Never,
            permission_profile: PermissionProfile::Disabled,
            sandbox_permissions: SandboxPermissions::UseDefault,
            prefix_rule: None,
        },
        ExecApprovalRequirement::Forbidden {
            reason: PROMPT_CONFLICT_REASON.to_string(),
        },
    )
    .await;
}

#[tokio::test]
async fn evaluates_powershell_inner_commands_against_allow_rules() {
    assert_exec_approval_requirement_for_command(
        ExecApprovalRequirementScenario {
            policy_src: Some(r#"prefix_rule(pattern=["echo"], decision="allow")"#.to_string()),
            command: vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "echo blocked".to_string(),
            ],
            approval_policy: AskForApproval::UnlessTrusted,
            permission_profile: PermissionProfile::read_only(),
            sandbox_permissions: SandboxPermissions::UseDefault,
            prefix_rule: None,
        },
        ExecApprovalRequirement::Skip {
            bypass_sandbox: true,
            proposed_execpolicy_amendment: None,
        },
    )
    .await;
}

#[test]
fn commands_for_exec_policy_parses_powershell_shell_wrapper() {
    let command = vec![
        "powershell.exe".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        "echo blocked".to_string(),
    ];

    assert_eq!(
        commands_for_exec_policy(&command),
        ExecPolicyCommands {
            commands: vec![vec!["echo".to_string(), "blocked".to_string()]],
            used_complex_parsing: false,
            command_origin: ExecPolicyCommandOrigin::PowerShell,
        }
    );
}

#[test]
fn unmatched_safe_powershell_words_are_allowed() {
    let command = vec!["Get-Content".to_string(), "Cargo.toml".to_string()];

    assert_eq!(
        Decision::Allow,
        render_decision_for_unmatched_command(
            &command,
            UnmatchedCommandContext {
                approval_policy: AskForApproval::UnlessTrusted,
                permission_profile: &PermissionProfile::read_only(),
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                sandbox_permissions: SandboxPermissions::UseDefault,
                used_complex_parsing: false,
                command_origin: ExecPolicyCommandOrigin::PowerShell,
            },
        )
    );
}

#[test]
fn read_only_windows_sandbox_runs_unmatched_commands_under_sandbox() {
    let command = vec!["cmd.exe".to_string(), "/c".to_string(), "dir".to_string()];

    for windows_sandbox_level in [
        WindowsSandboxLevel::RestrictedToken,
        WindowsSandboxLevel::Elevated,
    ] {
        assert_eq!(
            Decision::Allow,
            render_decision_for_unmatched_command(
                &command,
                UnmatchedCommandContext {
                    approval_policy: AskForApproval::Never,
                    permission_profile: &PermissionProfile::read_only(),
                    windows_sandbox_level,
                    sandbox_permissions: SandboxPermissions::UseDefault,
                    used_complex_parsing: false,
                    command_origin: ExecPolicyCommandOrigin::Generic,
                },
            )
        );
    }
}

#[test]
fn read_only_windows_policy_without_sandbox_backend_still_requires_approval() {
    let command = vec!["cmd.exe".to_string(), "/c".to_string(), "dir".to_string()];

    assert_eq!(
        Decision::Forbidden,
        render_decision_for_unmatched_command(
            &command,
            UnmatchedCommandContext {
                approval_policy: AskForApproval::Never,
                permission_profile: &PermissionProfile::read_only(),
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                sandbox_permissions: SandboxPermissions::UseDefault,
                used_complex_parsing: false,
                command_origin: ExecPolicyCommandOrigin::Generic,
            },
        ),
        "command is forbidden because approval policy is never and there is no Windows sandbox to rely on"
    );
}

#[test]
fn writable_windows_policy_without_sandbox_backend_still_requires_approval() {
    let command = vec!["cmd.exe".to_string(), "/c".to_string(), "dir".to_string()];
    let file_system_sandbox_policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
        },
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
            },
            access: FileSystemAccessMode::Write,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_sandbox_policy,
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        Decision::Forbidden,
        render_decision_for_unmatched_command(
            &command,
            UnmatchedCommandContext {
                approval_policy: AskForApproval::Never,
                permission_profile: &permission_profile,
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                sandbox_permissions: SandboxPermissions::UseDefault,
                used_complex_parsing: false,
                command_origin: ExecPolicyCommandOrigin::Generic,
            },
        )
    );
}

#[tokio::test]
async fn unmatched_dangerous_powershell_inner_commands_require_approval() {
    let inner_command = vec![
        "Remove-Item".to_string(),
        "test".to_string(),
        "-Force".to_string(),
    ];

    assert_exec_approval_requirement_for_command(
        ExecApprovalRequirementScenario {
            policy_src: None,
            command: vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Remove-Item test -Force".to_string(),
            ],
            approval_policy: AskForApproval::OnRequest,
            permission_profile: PermissionProfile::Disabled,
            sandbox_permissions: SandboxPermissions::UseDefault,
            prefix_rule: None,
        },
        ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: Some(ExecPolicyAmendment::new(inner_command)),
        },
    )
    .await;
}

#[cfg(windows)]
#[test]
fn powershell_policy_classification_yields_and_preserves_decisions_after_cancellation() {
    use std::time::Duration;

    async fn classify(
        manager: &ExecPolicyManager,
        command: &[String],
        command_for_safety: Option<&[String]>,
        direct: bool,
    ) -> ExecApprovalRequirement {
        let request = ExecApprovalRequest {
            command,
            command_for_safety,
            approval_policy: AskForApproval::OnRequest,
            permission_profile: PermissionProfile::read_only(),
            windows_sandbox_level: WindowsSandboxLevel::RestrictedToken,
            sandbox_permissions: SandboxPermissions::UseDefault,
            prefix_rule: None,
        };
        if direct {
            manager
                .create_exec_approval_requirement_for_direct_argv(request)
                .await
        } else {
            manager
                .create_exec_approval_requirement_for_command(request)
                .await
        }
    }

    assert!(
        codex_shell_command::powershell::is_trusted_powershell_executable("powershell.exe"),
        "this Windows parser behavior test requires the trusted Windows PowerShell host"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single-worker classification runtime");
    runtime.block_on(async {
        let mut parser = PolicyParser::new();
        parser
            .parse(
                "worker.rules",
                r#"
prefix_rule(pattern=["git", "status"], decision="allow")
prefix_rule(pattern=["git", "clean"], decision="forbidden")
prefix_rule(pattern=["powershell.exe"], decision="allow")
"#,
            )
            .expect("parse independent policy expectations");
        let manager = Arc::new(ExecPolicyManager::new(Arc::new(parser.build())));
        let original_policy = manager.current();
        let allowed = vec_str(&["powershell.exe", "-NoProfile", "-Command", "git status"]);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (occupied_tx, occupied_rx) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            occupied_tx.send(()).expect("worker occupied notification");
            release_rx.recv().expect("release occupied worker");
        });
        occupied_rx.await.expect("blocking worker occupied");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let classification_manager = Arc::clone(&manager);
        let classification_command = allowed.clone();
        let classification = tokio::spawn(async move {
            entered_tx.send(()).expect("classification entered");
            classify(
                &classification_manager,
                &classification_command,
                None,
                false,
            )
            .await
        });
        entered_rx
            .await
            .expect("normal classification task started");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !classification.is_finished(),
            "normal classification must await its worker while the runtime timer progresses"
        );
        classification.abort();
        assert!(
            classification
                .await
                .expect_err("caller cancelled")
                .is_cancelled()
        );
        assert!(
            Arc::ptr_eq(&manager.current(), &original_policy),
            "classification cancellation must not mutate policy"
        );
        release_tx.send(()).expect("release worker");
        blocker.await.expect("occupied worker exited");

        let expected_allow = ExecApprovalRequirement::Skip {
            bypass_sandbox: true,
            proposed_execpolicy_amendment: None,
        };
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(20),
                classify(&manager, &allowed, None, false)
            )
            .await
            .expect("real PowerShell parsing finishes after cancellation"),
            expected_allow,
        );
        let mixed = vec_str(&[
            "powershell.exe",
            "-NoProfile",
            "-Command",
            "git status; git clean -fd",
        ]);
        let denied = classify(&manager, &mixed, None, false).await;
        let ExecApprovalRequirement::Forbidden { reason } = denied else {
            panic!("the forbidden second parsed command must dominate the allowed first command");
        };
        assert!(
            reason.contains("git clean"),
            "denial must identify the forbidden parsed command: {reason}"
        );
        // Direct argv is owned by the caller and must not be reinterpreted as shell source.
        assert_eq!(
            classify(&manager, &mixed, Some(&allowed), true).await,
            expected_allow
        );
        let encoded = vec_str(&["powershell.exe", "-EncodedCommand", "not-executed"]);
        assert_eq!(
            classify(&manager, &encoded, Some(&allowed), false).await,
            expected_allow
        );
        assert!(Arc::ptr_eq(&manager.current(), &original_policy));
    });
}
