use super::*;
use codex_exec_server::Environment;
use codex_utils_path_uri::PathUri;
use std::sync::Arc;

fn shell_request(environment_id: &str) -> ShellRequest {
    let cwd = AbsolutePathBuf::try_from(std::env::current_dir().expect("read current dir"))
        .expect("current dir is absolute");
    ShellRequest {
        command: vec!["echo".to_string(), "hello".to_string()],
        command_for_approval: vec!["echo".to_string(), "hello".to_string()],

        approved_powershell_direct_argv: None,
        turn_environment: TurnEnvironment::new(
            environment_id.to_string(),
            Arc::new(Environment::default_for_tests()),
            PathUri::from_abs_path(&cwd),
            /*shell*/ None,
        ),
        shell_type: None,
        hook_command: "echo hello".to_string(),
        cwd,
        timeout_ms: None,
        stall_timeout_ms: None,
        cancellation_token: CancellationToken::new(),
        env: HashMap::new(),
        explicit_env_overrides: HashMap::new(),
        network: None,
        sandbox_permissions: SandboxPermissions::UseDefault,
        additional_permissions: None,
        justification: None,
        exec_approval_requirement: ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
        validation_launch: false,
        validation_proof: false,
        workspace_operation_root: None,
    }
}

#[tokio::test]
async fn approval_key_includes_environment_id_and_approval_scope() {
    let mut request = shell_request("remote");
    let runtime = ShellRuntime::for_shell_command();
    let original_key = runtime.approval_keys(&request);
    request.turn_environment.environment = Arc::new(Environment::default_for_tests());
    let replacement_key = runtime.approval_keys(&request);
    assert_ne!(original_key, replacement_key);

    request.turn_environment.environment_id = "other".to_string();
    let other_key = runtime.approval_keys(&request);

    assert_ne!(replacement_key, other_key);
}

#[tokio::test]
async fn approval_key_uses_inspectable_command_instead_of_encoded_payload() {
    let request = ShellRequest {
        command: vec![
            "pwsh".to_string(),
            "-EncodedCommand".to_string(),
            "RwBlAHQALQBDAGgAaQBsAGQASQB0AGUAbQA=".to_string(),
        ],
        command_for_approval: vec![
            "pwsh".to_string(),
            "-Command".to_string(),
            "Get-ChildItem".to_string(),
        ],

        shell_type: Some(ShellType::PowerShell),
        hook_command: "Get-ChildItem".to_string(),
        ..shell_request("local")
    };
    let runtime = ShellRuntime::for_shell_command();

    let keys = runtime.approval_keys(&request);
    assert_eq!(keys.len(), 1);
    assert_eq!(
        keys[0].command,
        vec![
            "__codex_powershell_script__",
            "pwsh",
            "profiles-enabled",
            "Get-ChildItem"
        ]
    );
    assert_ne!(
        keys[0].command,
        canonicalize_command_for_approval(&request.command)
    );
}

#[test]
fn validation_sandbox_attempt_output_is_retained_for_terminal_retry_errors() {
    let mut runtime = ShellRuntime::for_shell_command();
    let error = CodexErr::Sandbox(SandboxErr::Denied {
        output: Box::new(ExecToolCallOutput {
            exit_code: 126,
            aggregated_output: codex_protocol::exec_output::StreamOutput::new(
                "sandbox denied".to_string(),
            ),
            ..Default::default()
        }),
        network_policy_decision: None,
    });

    runtime.remember_validation_attempt_error(true, true, &error);

    let output = runtime
        .take_last_validation_attempt_output()
        .expect("the executed validation attempt must remain observable");
    assert_eq!(output.exit_code, 126);
    assert_eq!(output.aggregated_output.text, "sandbox denied");
}

#[test]
fn post_spawn_validation_error_retains_the_execution_boundary() {
    let mut runtime = ShellRuntime::for_shell_command();
    let error = CodexErr::Io(std::io::Error::other("output reader failed"));

    runtime.remember_validation_attempt_error(true, true, &error);

    assert!(runtime.take_last_validation_attempt_started());
    assert!(runtime.take_last_validation_attempt_output().is_none());
}

#[tokio::test(start_paused = true)]
async fn command_progress_resets_stall_deadline() {
    let progress = crate::exec::CommandProgress::new();
    let observer = progress.subscribe();
    let stall = tokio::spawn(super::wait_for_command_stall(
        observer,
        std::time::Duration::from_secs(10),
    ));
    // Start the detector before advancing time; otherwise the first deadline
    // could start at t=9 and a detector that ignores progress would also pass.
    tokio::task::yield_now().await;

    tokio::time::advance(std::time::Duration::from_secs(9)).await;
    progress.record_output();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(9)).await;
    tokio::task::yield_now().await;
    assert!(!stall.is_finished());

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(stall.is_finished());
    stall
        .await
        .expect("stall detector exits at the reset deadline");
}

/// Like `build_exec_request` and unified exec, a launch under managed network whose policy
/// requested a sandbox this host cannot provide is refused before any setup. This runtime always
/// launches on this host, so a remote-labelled environment is refused too. A held validation
/// owner stops every other launch at admission, so the controls show the refusal applies to no
/// other case without starting a process.
#[tokio::test]
async fn managed_network_launch_without_available_sandbox_is_refused_before_setup() {
    let temp = tempfile::tempdir().expect("create workspace");
    let root = AbsolutePathBuf::try_from(temp.path().to_path_buf()).expect("absolute workspace");
    let cwd = PathUri::from_abs_path(&root);
    let holder = crate::workspace_operation_gate::ValidationOwner::for_command(
        codex_protocol::ThreadId::new(),
        &["cargo".to_string(), "test".to_string()],
    );
    let _owner = crate::workspace_operation_gate::acquire_workspace_validation(
        root.as_path(),
        "cargo test -p held-validation",
        holder,
    )
    .await
    .expect("hold the workspace validation gate");
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let ctx = ToolCtx {
        session: Arc::new(session),
        turn: Arc::new(turn),
        call_id: "managed-network-sandbox".to_string(),
        tool_name: codex_tools::ToolName::plain("shell_command"),
    };
    let local = ShellRequest {
        command: vec!["must-not-launch-sandbox-fixture".to_string()],
        command_for_approval: vec!["cargo".to_string(), "test".to_string()],
        turn_environment: TurnEnvironment::new(
            "local".to_string(),
            Arc::new(Environment::default_for_tests()),
            cwd.clone(),
            /*shell*/ None,
        ),
        hook_command: "cargo test".to_string(),
        cwd: root.clone(),
        workspace_operation_root: Some(root.to_path_buf()),
        ..shell_request("local")
    };
    let remote = ShellRequest {
        turn_environment: TurnEnvironment::new(
            "remote".to_string(),
            Arc::new(
                Environment::create_for_tests(Some("ws://127.0.0.1:1".to_string()))
                    .expect("remote-labelled environment"),
            ),
            cwd.clone(),
            /*shell*/ None,
        ),
        ..local.clone()
    };
    assert!(remote.turn_environment.environment.is_remote());
    let permissions = codex_protocol::models::PermissionProfile::read_only();
    let mut runtime = ShellRuntime::for_shell_command();
    for (request, enforce_managed_network, sandbox_requested, refused) in [
        (&local, true, true, true),
        // The launch is local even when the environment is labelled remote.
        (&remote, true, true, true),
        // Restrictions still require enforcement without a managed proxy.
        (&local, false, true, true),
        (&remote, false, true, true),
        // A launch whose policy requested no sandbox (an explicit bypass) is unchanged.
        (&local, true, false, false),
        (&local, false, false, false),
    ] {
        let case = (
            request.turn_environment.environment_id.as_str(),
            enforce_managed_network,
            sandbox_requested,
        );
        let attempt = SandboxAttempt {
            codex_home: &root,
            sandbox: codex_sandboxing::SandboxType::None,
            sandbox_requested,
            permissions: &permissions,
            exec_server_permissions: &permissions,
            enforce_managed_network,
            sandbox_cwd: &cwd,
            workspace_roots: std::slice::from_ref(&root),
            windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
            windows_sandbox_private_desktop: false,
            network_denial_cancellation_token: None,
            network_proxy: None,
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            runtime.run(request, &attempt, &ctx),
        )
        .await
        .expect("no case may wait for the held validation owner");
        if refused {
            let Err(ToolError::Codex(CodexErr::Io(error))) = &result else {
                panic!("{case:?}: expected the unavailable-sandbox refusal, got {result:?}");
            };
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{case:?}"
            );
            assert_eq!(
                error.to_string(),
                "requested restrictions require a sandbox, but no sandbox backend is available; refusing to run unsandboxed",
                "{case:?}"
            );
        } else {
            assert!(
                matches!(&result, Err(ToolError::Rejected(message))
                    if message.contains("No process was started or queued")),
                "{case:?}: the launch must reach validation admission, got {result:?}"
            );
        }
    }
}
