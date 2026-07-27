use super::*;
use codex_exec_server::Environment;
use codex_utils_path_uri::PathUri;
use std::sync::Arc;

#[tokio::test]
async fn approval_key_includes_environment_id() {
    let cwd = AbsolutePathBuf::try_from(std::env::current_dir().expect("read current dir"))
        .expect("current dir is absolute");
    let mut request = ShellRequest {
        command: vec!["echo".to_string(), "hello".to_string()],
        command_for_approval: vec!["echo".to_string(), "hello".to_string()],
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
        cancellation_token: CancellationToken::new(),
        env: HashMap::new(),
        explicit_env_overrides: HashMap::new(),
        network: None,
        sandbox_permissions: SandboxPermissions::UseDefault,
        additional_permissions: None,
        #[cfg(unix)]
        additional_permissions_preapproved: false,
        justification: None,
        exec_approval_requirement: ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
    };
    let runtime = ShellRuntime::for_shell_command(ShellRuntimeBackend::ShellCommandClassic);
    let original_key = runtime.approval_keys(&request);
    request.turn_environment.environment_id = "other".to_string();
    let other_key = runtime.approval_keys(&request);

    assert_ne!(original_key, other_key);
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
        cancellation_token: CancellationToken::new(),
        env: HashMap::new(),
        explicit_env_overrides: HashMap::new(),
        network: None,
        sandbox_permissions: SandboxPermissions::UseDefault,
        additional_permissions: None,
        #[cfg(unix)]
        additional_permissions_preapproved: false,
        justification: None,
        exec_approval_requirement: ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
    };
    let runtime = ShellRuntime::for_shell_command(ShellRuntimeBackend::ShellCommandClassic);

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
