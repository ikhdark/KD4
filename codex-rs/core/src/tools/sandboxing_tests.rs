use super::*;
use crate::sandboxing::SandboxPermissions;
use crate::tools::hook_names::HookToolName;
use codex_network_proxy::ManagedNetworkSandboxContext;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::GranularApprovalConfig;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxType;
use codex_sandboxing::WindowsSandboxFilesystemOverrides;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;

#[test]
fn bash_permission_request_payload_omits_missing_description() {
    assert_eq!(
        PermissionRequestPayload::bash("echo hi".to_string(), /*description*/ None),
        PermissionRequestPayload {
            tool_name: HookToolName::bash(),
            tool_input: json!({ "command": "echo hi" }),
        }
    );
}

#[test]
fn bash_permission_request_payload_includes_description_when_present() {
    assert_eq!(
        PermissionRequestPayload::bash(
            "echo hi".to_string(),
            Some("network-access example.com".to_string()),
        ),
        PermissionRequestPayload {
            tool_name: HookToolName::bash(),
            tool_input: json!({
                "command": "echo hi",
                "description": "network-access example.com",
            }),
        }
    );
}

#[test]
fn external_sandbox_skips_exec_approval_on_request() {
    assert_eq!(
        default_exec_approval_requirement(
            AskForApproval::OnRequest,
            &FileSystemSandboxPolicy::external_sandbox(),
        ),
        ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        }
    );
}

#[test]
fn restricted_sandbox_requires_exec_approval_on_request() {
    assert_eq!(
        default_exec_approval_requirement(
            AskForApproval::OnRequest,
            &FileSystemSandboxPolicy::default()
        ),
        ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: None,
        }
    );
}

#[test]
fn default_exec_approval_requirement_rejects_sandbox_prompt_when_granular_disables_it() {
    let policy = AskForApproval::Granular(GranularApprovalConfig {
        sandbox_approval: false,
        rules: true,
        skill_approval: true,
        request_permissions: true,
        mcp_elicitations: true,
    });

    let requirement =
        default_exec_approval_requirement(policy, &FileSystemSandboxPolicy::default());

    assert_eq!(
        requirement,
        ExecApprovalRequirement::Forbidden {
            reason: "approval policy disallowed sandbox approval prompt".to_string(),
        }
    );
}

#[test]
fn default_exec_approval_requirement_keeps_prompt_when_granular_allows_sandbox_approval() {
    let policy = AskForApproval::Granular(GranularApprovalConfig {
        sandbox_approval: true,
        rules: false,
        skill_approval: true,
        request_permissions: true,
        mcp_elicitations: false,
    });

    let requirement =
        default_exec_approval_requirement(policy, &FileSystemSandboxPolicy::default());

    assert_eq!(
        requirement,
        ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: None,
        }
    );
}

#[test]
fn additional_permissions_allow_bypass_sandbox_first_attempt_when_execpolicy_skips() {
    assert_eq!(
        sandbox_override_for_first_attempt(
            SandboxPermissions::WithAdditionalPermissions,
            &ExecApprovalRequirement::Skip {
                bypass_sandbox: true,
                proposed_execpolicy_amendment: None,
            },
            &FileSystemSandboxPolicy::default(),
        ),
        SandboxOverride::BypassSandboxFirstAttempt
    );
}

#[test]
fn guardian_bypasses_sandbox_for_explicit_escalation_on_first_attempt() {
    assert_eq!(
        sandbox_override_for_first_attempt(
            SandboxPermissions::RequireEscalated,
            &ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
            &FileSystemSandboxPolicy::default(),
        ),
        SandboxOverride::BypassSandboxFirstAttempt
    );
}

#[test]
fn deny_read_blocks_explicit_escalation_and_policy_bypass() {
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
        path: FileSystemPath::GlobPattern {
            pattern: "**/*.env".to_string(),
        },
        access: FileSystemAccessMode::Deny,
    }]);

    assert_eq!(
        sandbox_override_for_first_attempt(
            SandboxPermissions::RequireEscalated,
            &ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
            &file_system_policy,
        ),
        SandboxOverride::NoOverride,
        "explicit escalation would drop deny-read filesystem policy, so keep the first attempt sandboxed",
    );
    assert!(!unsandboxed_execution_allowed(&file_system_policy));
    assert_eq!(
        sandbox_permissions_preserving_denied_reads(
            SandboxPermissions::RequireEscalated,
            &file_system_policy,
        ),
        SandboxPermissions::UseDefault,
    );
    assert_eq!(
        sandbox_permissions_preserving_denied_reads(
            SandboxPermissions::WithAdditionalPermissions,
            &file_system_policy,
        ),
        SandboxPermissions::WithAdditionalPermissions,
    );
    assert_eq!(
        sandbox_permissions_preserving_denied_reads(
            SandboxPermissions::RequireEscalated,
            &FileSystemSandboxPolicy::default(),
        ),
        SandboxPermissions::RequireEscalated,
    );
    assert_eq!(
        sandbox_override_for_first_attempt(
            SandboxPermissions::WithAdditionalPermissions,
            &ExecApprovalRequirement::Skip {
                bypass_sandbox: true,
                proposed_execpolicy_amendment: None,
            },
            &file_system_policy,
        ),
        SandboxOverride::NoOverride,
        "exec-policy allow rules would drop deny-read filesystem policy, so keep the first attempt sandboxed",
    );
}

#[test]
fn exec_server_env_keeps_command_native_and_carries_sandbox_context() {
    let cwd: AbsolutePathBuf = std::env::current_dir()
        .expect("current dir")
        .try_into()
        .expect("absolute cwd");
    let cwd_uri = PathUri::from_abs_path(&cwd);
    let exec_server_permissions = codex_protocol::models::PermissionProfile::workspace_write();
    let permissions = exec_server_permissions
        .clone()
        .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&cwd));
    let mut attempt = SandboxAttempt {
        sandbox: SandboxType::None,
        sandbox_requested: true,
        permissions: &permissions,
        exec_server_permissions: &exec_server_permissions,
        enforce_managed_network: true,
        sandbox_cwd: &cwd_uri,
        workspace_roots: std::slice::from_ref(&cwd),
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: false,
        network_denial_cancellation_token: None,
        network_proxy: None,
    };
    let managed_network = ManagedNetworkSandboxContext {
        loopback_ports: vec![43123],
        allow_local_binding: false,
    };
    let command = || SandboxCommand {
        program: "/bin/bash".into(),
        args: vec!["-lc".to_string(), "pwd".to_string()],
        cwd: cwd_uri.clone(),
        env: HashMap::new(),
        managed_network: Some(managed_network.clone()),
        additional_permissions: None,
    };
    let options = || crate::sandboxing::ExecOptions {
        expiration: crate::exec::ExecExpiration::DefaultTimeout,
        capture_policy: crate::exec::ExecCapturePolicy::ShellTool,
    };
    let request = attempt
        .env_for_exec_server(command(), options(), /*network*/ None, Some("remote"))
        .expect("prepare remote exec request");

    assert_eq!(
        request.command,
        vec![
            "/bin/bash".to_string(),
            "-lc".to_string(),
            "pwd".to_string()
        ]
    );
    assert_eq!(request.arg0, None);
    assert_eq!(request.sandbox, SandboxType::None);
    assert_eq!(
        request.exec_server_sandbox,
        Some(codex_exec_server::FileSystemSandboxContext {
            permissions: exec_server_permissions.clone().into(),
            cwd: Some(cwd_uri.clone()),
            workspace_roots: Vec::new(),
            windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
            windows_sandbox_private_desktop: false,
        })
    );
    assert!(request.exec_server_enforce_managed_network);
    assert_eq!(
        request.exec_server_managed_network,
        Some(managed_network.clone())
    );

    attempt.sandbox_requested = false;
    let request = attempt
        .env_for_exec_server(command(), options(), /*network*/ None, Some("remote"))
        .expect("prepare unsandboxed remote exec request");

    assert_eq!(request.exec_server_sandbox, None);
    assert!(!request.exec_server_enforce_managed_network);
    assert_eq!(request.exec_server_managed_network, Some(managed_network));
}

#[test]
fn local_env_carries_restricted_token_filesystem_overrides() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        dunce::canonicalize(temp_dir.path()).expect("canonical cwd"),
    )
    .expect("absolute cwd");
    let docs = cwd.join("docs");
    std::fs::create_dir_all(docs.as_path()).expect("create docs");
    let permissions = codex_protocol::models::PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(vec![
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
            FileSystemSandboxEntry {
                path: FileSystemPath::Path { path: docs.clone() },
                access: FileSystemAccessMode::Read,
            },
        ]),
        NetworkSandboxPolicy::Restricted,
    );
    let cwd_uri = PathUri::from_abs_path(&cwd);
    let attempt = SandboxAttempt {
        sandbox: SandboxType::WindowsRestrictedToken,
        sandbox_requested: true,
        permissions: &permissions,
        exec_server_permissions: &permissions,
        enforce_managed_network: false,
        sandbox_cwd: &cwd_uri,
        workspace_roots: std::slice::from_ref(&cwd),
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::RestrictedToken,
        windows_sandbox_private_desktop: false,
        network_denial_cancellation_token: None,
        network_proxy: None,
    };
    let request = attempt
        .env_for(
            SandboxCommand {
                program: "cmd.exe".into(),
                args: vec!["/c".to_string(), "exit 0".to_string()],
                cwd: cwd_uri.clone(),
                env: HashMap::new(),
                managed_network: None,
                additional_permissions: None,
            },
            crate::sandboxing::ExecOptions {
                expiration: crate::exec::ExecExpiration::DefaultTimeout,
                capture_policy: crate::exec::ExecCapturePolicy::ShellTool,
            },
            /*network*/ None,
            /*environment_id*/ None,
        )
        .expect("prepare local exec request");

    assert_eq!(
        request.windows_sandbox_filesystem_overrides,
        Some(WindowsSandboxFilesystemOverrides {
            read_roots_override: None,
            read_roots_include_platform_defaults: false,
            write_roots_override: None,
            additional_deny_read_paths: vec![],
            additional_deny_write_paths: vec![docs],
        })
    );
}

#[test]
fn local_env_carries_elevated_filesystem_overrides() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        dunce::canonicalize(temp_dir.path()).expect("canonical cwd"),
    )
    .expect("absolute cwd");
    let docs = cwd.join("docs");
    std::fs::create_dir_all(docs.as_path()).expect("create docs");
    let permissions = codex_protocol::models::PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: docs.clone() },
            access: FileSystemAccessMode::Read,
        }]),
        NetworkSandboxPolicy::Restricted,
    );
    let cwd_uri = PathUri::from_abs_path(&cwd);
    let attempt = SandboxAttempt {
        sandbox: SandboxType::WindowsRestrictedToken,
        sandbox_requested: true,
        permissions: &permissions,
        exec_server_permissions: &permissions,
        enforce_managed_network: false,
        sandbox_cwd: &cwd_uri,
        workspace_roots: std::slice::from_ref(&cwd),
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Elevated,
        windows_sandbox_private_desktop: false,
        network_denial_cancellation_token: None,
        network_proxy: None,
    };
    let request = attempt
        .env_for(
            SandboxCommand {
                program: "cmd.exe".into(),
                args: vec!["/c".to_string(), "exit 0".to_string()],
                cwd: cwd_uri.clone(),
                env: HashMap::new(),
                managed_network: None,
                additional_permissions: None,
            },
            crate::sandboxing::ExecOptions {
                expiration: crate::exec::ExecExpiration::DefaultTimeout,
                capture_policy: crate::exec::ExecCapturePolicy::ShellTool,
            },
            /*network*/ None,
            /*environment_id*/ None,
        )
        .expect("prepare local exec request");

    assert_eq!(
        request.windows_sandbox_filesystem_overrides,
        Some(WindowsSandboxFilesystemOverrides {
            read_roots_override: Some(vec![docs.into_path_buf()]),
            read_roots_include_platform_defaults: false,
            write_roots_override: None,
            additional_deny_read_paths: vec![],
            additional_deny_write_paths: vec![],
        })
    );
}
