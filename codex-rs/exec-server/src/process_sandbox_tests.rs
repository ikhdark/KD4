use std::collections::HashMap;

use codex_network_proxy::ManagedNetworkSandboxContext;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;

use super::prepare_exec_request;
use super::windows_sandbox_command;
use super::windows_sandbox_stdin_open;
use crate::ExecParams;
use crate::ExecServerRuntimePaths;
use crate::FileSystemSandboxContext;
use crate::ProcessId;

#[test]
fn native_request_preserves_native_launch_fields() {
    let cwd: AbsolutePathBuf = std::env::current_dir()
        .expect("current directory")
        .try_into()
        .expect("absolute cwd");
    let cwd_uri = PathUri::from_abs_path(&cwd);
    let env = HashMap::from([("TEST_ENV".to_string(), "value".to_string())]);
    let params = ExecParams {
        process_id: ProcessId::from("process-1"),
        argv: vec!["echo".to_string(), "hello".to_string()],
        cwd: cwd_uri,
        env_policy: None,
        env: HashMap::new(),
        tty: false,
        pipe_stdin: false,
        arg0: Some("custom-arg0".to_string()),
        sandbox: None,
        enforce_managed_network: false,
        managed_network: None,
    };

    let prepared = prepare_exec_request(&params, env.clone(), /*runtime_paths*/ None)
        .expect("prepare native request");

    assert_eq!(prepared.command, params.argv);
    assert_eq!(prepared.cwd, cwd);
    assert_eq!(prepared.env, env);
    assert_eq!(prepared.arg0, params.arg0);
}

#[test]
fn windows_restricted_token_request_prepares_session_launch() {
    let cwd: AbsolutePathBuf = std::env::current_dir()
        .expect("current directory")
        .try_into()
        .expect("absolute cwd");
    let cwd_uri = PathUri::from_abs_path(&cwd);
    let self_exe = std::env::current_exe().expect("current executable");
    let runtime_paths = ExecServerRuntimePaths::new(self_exe.clone()).expect("runtime paths");
    let mut sandbox = FileSystemSandboxContext::from_permission_profile_with_cwd(
        PermissionProfile::workspace_write(),
        cwd_uri.clone(),
    );
    sandbox.windows_sandbox_level = WindowsSandboxLevel::RestrictedToken;
    sandbox.windows_sandbox_private_desktop = true;
    let env = HashMap::from([("TEST_ENV".to_string(), "value".to_string())]);
    let params = ExecParams {
        process_id: ProcessId::from("windows-sandbox-process"),
        argv: vec![
            self_exe.to_string_lossy().into_owned(),
            "--child".to_string(),
            "value".to_string(),
        ],
        cwd: cwd_uri,
        env_policy: None,
        env: HashMap::new(),
        tty: true,
        pipe_stdin: false,
        arg0: Some("custom-tty-program".to_string()),
        sandbox: Some(sandbox),
        enforce_managed_network: true,
        managed_network: Some(ManagedNetworkSandboxContext {
            loopback_ports: vec![43123],
            allow_local_binding: false,
        }),
    };

    let prepared = prepare_exec_request(&params, env.clone(), Some(&runtime_paths))
        .expect("prepare restricted-token request");

    assert_eq!(prepared.command, params.argv);
    assert_eq!(prepared.cwd, cwd);
    assert_eq!(prepared.env, env);
    assert_eq!(prepared.arg0, params.arg0);
    let windows_sandbox = prepared
        .windows_sandbox
        .as_ref()
        .expect("Windows session launch metadata");
    assert_eq!(
        windows_sandbox.permission_profile,
        PermissionProfile::workspace_write()
            .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&cwd))
    );
    assert_eq!(windows_sandbox.workspace_roots, vec![cwd]);
    assert_eq!(
        windows_sandbox.windows_sandbox_level,
        WindowsSandboxLevel::RestrictedToken
    );
    assert!(windows_sandbox.proxy_enforced);
    assert!(windows_sandbox.use_private_desktop);

    assert_eq!(
        windows_sandbox_command(
            prepared.command.clone(),
            prepared.arg0.clone(),
            /*tty*/ true,
        )
        .expect("PTY launch command"),
        vec![
            "custom-tty-program".to_string(),
            "--child".to_string(),
            "value".to_string(),
        ]
    );
    assert_eq!(
        windows_sandbox_command(
            prepared.command.clone(),
            prepared.arg0.clone(),
            /*tty*/ false,
        )
        .expect("pipe launch command"),
        prepared.command
    );
    assert!(windows_sandbox_stdin_open(
        /*tty*/ true, /*pipe_stdin*/ false
    ));
    assert!(windows_sandbox_stdin_open(
        /*tty*/ false, /*pipe_stdin*/ true
    ));
    assert!(!windows_sandbox_stdin_open(
        /*tty*/ false, /*pipe_stdin*/ false,
    ));
}
