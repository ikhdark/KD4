use super::*;
use codex_exec_server::Environment;
use codex_utils_path_uri::PathUri;
use std::sync::Arc;

#[tokio::test]
async fn approval_key_includes_environment_id_and_approval_scope() {
    let cwd = AbsolutePathBuf::try_from(std::env::current_dir().expect("read current dir"))
        .expect("current dir is absolute");
    let mut request = ShellRequest {
        command: vec!["echo".to_string(), "hello".to_string()],
        command_for_approval: vec!["echo".to_string(), "hello".to_string()],

        approved_powershell_direct_argv: None,
        turn_environment: TurnEnvironment::new(
            "remote".to_string(),
            Arc::new(Environment::default_for_tests()),
            PathUri::from_abs_path(&cwd),
            /*shell*/ None,
        ),
        shell_type: None,
        hook_command: "echo hello".to_string(),
        cwd: cwd.clone(),
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
        known_delta: None,
        validation_launch: None,
        workspace_operation_root: None,
    };
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
    let cwd = AbsolutePathBuf::try_from(std::env::current_dir().expect("read current dir"))
        .expect("current dir is absolute");
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

        approved_powershell_direct_argv: None,
        turn_environment: TurnEnvironment::new(
            "local".to_string(),
            Arc::new(Environment::default_for_tests()),
            PathUri::from_abs_path(&cwd),
            /*shell*/ None,
        ),
        shell_type: Some(ShellType::PowerShell),
        hook_command: "Get-ChildItem".to_string(),
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
        known_delta: None,
        validation_launch: None,
        workspace_operation_root: None,
    };
    let runtime = ShellRuntime::for_shell_command();

    let keys = runtime.approval_keys(&request);
    assert_eq!(keys.len(), 1);
    assert_eq!(
        keys[0].command,
        canonicalize_command_for_approval(&request.command_for_approval)
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
