use super::*;
use crate::session::tests::make_session_and_context_with_rx;
use crate::state::ActiveTurn;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::SandboxAttempt;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GranularApprovalConfig;
use codex_sandboxing::SandboxType;
use codex_sandboxing::policy_transforms::effective_file_system_sandbox_policy;
use codex_sandboxing::policy_transforms::effective_network_sandbox_policy;
use codex_utils_path_uri::PathUri;
use core_test_support::PathBufExt;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
fn test_turn_environment(environment_id: &str) -> crate::session::turn_context::TurnEnvironment {
    crate::session::turn_context::TurnEnvironment::new(
        environment_id.to_string(),
        std::sync::Arc::new(codex_exec_server::Environment::default_for_tests()),
        PathUri::from_abs_path(&std::env::temp_dir().abs()),
        /*shell*/ None,
    )
}

#[test]
fn wants_no_sandbox_approval_granular_respects_sandbox_flag() {
    let runtime = ApplyPatchRuntime::new();
    assert!(runtime.wants_no_sandbox_approval(AskForApproval::OnRequest));
    assert!(
        !runtime.wants_no_sandbox_approval(AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: false,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: true,
        }))
    );
    assert!(
        runtime.wants_no_sandbox_approval(AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: true,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: true,
        }))
    );
}

#[tokio::test]
async fn guardian_review_request_includes_patch_context() {
    let path = std::env::temp_dir()
        .join("guardian-apply-patch-test.txt")
        .abs();
    let action =
        ApplyPatchAction::new_add_for_test(&PathUri::from_abs_path(&path), "hello".to_string());
    let expected_cwd = action.cwd.clone();
    let expected_patch = action.patch.clone();
    let request = ApplyPatchRequest {
        turn_environment: test_turn_environment(codex_exec_server::LOCAL_ENVIRONMENT_ID),
        action,
        file_paths: vec![PathUri::from_abs_path(&path)],
        changes: HashMap::from([(
            path.to_path_buf(),
            FileChange::Add {
                content: "hello".to_string(),
            },
        )]),
        exec_approval_requirement: ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: None,
        permissions_preapproved: false,
    };

    let guardian_request = ApplyPatchRuntime::build_guardian_review_request(&request, "call-1")
        .expect("native guardian request cwd");

    assert_eq!(
        guardian_request,
        ApprovalAction::ApplyPatch {
            id: "call-1".to_string(),
            cwd: expected_cwd,
            files: vec![PathUri::from_abs_path(&path)],
            patch: expected_patch,
        }
    );
}

#[tokio::test]
async fn guardian_review_request_preserves_foreign_paths() {
    let path = PathUri::parse("file:///tmp/guardian-remote.txt").expect("POSIX path URI");
    let action = ApplyPatchAction::new_add_for_test(&path, "hello".to_string());
    let expected_cwd = action.cwd.clone();
    let expected_patch = action.patch.clone();
    let request = ApplyPatchRequest {
        turn_environment: test_turn_environment("remote"),
        action,
        file_paths: vec![path.clone()],
        changes: HashMap::new(),
        exec_approval_requirement: ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: None,
        permissions_preapproved: false,
    };

    let guardian_request =
        ApplyPatchRuntime::build_guardian_review_request(&request, "call-remote")
            .expect("foreign guardian paths should not require host conversion");

    assert_eq!(
        guardian_request,
        ApprovalAction::ApplyPatch {
            id: "call-remote".to_string(),
            cwd: expected_cwd,
            files: vec![path],
            patch: expected_patch,
        }
    );
}

#[test]
fn unbound_mutation_evidence_allows_paths_outside_the_repo() {
    let repo_root = std::env::temp_dir().join("apply-patch-repo").abs();
    let outside_path = std::env::temp_dir().join("apply-patch-outside.txt").abs();
    let outside_path = PathUri::from_abs_path(&outside_path);

    let evidence = native_mutation_repo_paths(
        repo_root.as_path(),
        std::slice::from_ref(&outside_path),
        /*require_complete*/ false,
    )
    .expect("best-effort evidence must not block an otherwise approved patch");
    assert_eq!(
        evidence,
        NativeMutationRepoPaths {
            paths: Vec::new(),
            complete: false,
        }
    );

    let error = native_mutation_repo_paths(
        repo_root.as_path(),
        &[outside_path],
        /*require_complete*/ true,
    )
    .expect_err("bound assignments still require complete mutation evidence");
    assert!(format!("{error:?}").contains("outside the evidence workspace"));
}

#[tokio::test]
async fn permission_request_payload_uses_apply_patch_hook_name_and_aliases() {
    let runtime = ApplyPatchRuntime::new();
    let path = std::env::temp_dir()
        .join("apply-patch-permission-request-payload.txt")
        .abs();
    let action =
        ApplyPatchAction::new_add_for_test(&PathUri::from_abs_path(&path), "hello".to_string());
    let expected_patch = action.patch.clone();
    let req = ApplyPatchRequest {
        turn_environment: test_turn_environment(codex_exec_server::LOCAL_ENVIRONMENT_ID),
        action,
        file_paths: vec![PathUri::from_abs_path(&path)],
        changes: HashMap::new(),
        exec_approval_requirement: ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: None,
        permissions_preapproved: false,
    };

    let payload = runtime
        .permission_request_payload(&req)
        .expect("permission request payload");

    assert_eq!(payload.tool_name.name(), "apply_patch");
    assert_eq!(
        payload.tool_name.matcher_aliases(),
        &["Write".to_string(), "Edit".to_string()]
    );
    assert_eq!(
        payload.tool_input,
        serde_json::json!({ "command": expected_patch })
    );
}

#[tokio::test]
async fn approval_keys_include_environment_id_and_approval_scope() {
    let runtime = ApplyPatchRuntime::new();
    let path = std::env::temp_dir()
        .join("apply-patch-approval-key.txt")
        .abs();
    let path_uri = PathUri::from_abs_path(&path);
    let mut req = ApplyPatchRequest {
        turn_environment: test_turn_environment("remote"),
        action: ApplyPatchAction::new_add_for_test(&path_uri, "hello".to_string()),
        file_paths: vec![path_uri.clone()],
        changes: HashMap::new(),
        exec_approval_requirement: ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: None,
        permissions_preapproved: false,
    };

    let keys = runtime.approval_keys(&req);
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].environment_id, "remote");
    assert_eq!(keys[0].path, path_uri);
    assert_eq!(
        keys[0].approval_scope_id,
        req.turn_environment.environment.approval_scope_id()
    );

    req.turn_environment = test_turn_environment("remote");
    let replacement_keys = runtime.approval_keys(&req);
    assert_ne!(keys, replacement_keys);
}

#[tokio::test]
async fn sandbox_retry_session_approval_is_cached_separately() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let path = std::env::temp_dir()
        .join("apply-patch-retry-approval-cache.txt")
        .abs();
    let path_uri = PathUri::from_abs_path(&path);

    let approvals = tokio::spawn({
        let session = session.clone();
        let turn = turn.clone();
        async move {
            let req = ApplyPatchRequest {
                turn_environment: test_turn_environment("remote"),
                action: ApplyPatchAction::new_add_for_test(&path_uri, "hello".to_string()),
                file_paths: vec![path_uri],
                changes: HashMap::from([(
                    path.to_path_buf(),
                    FileChange::Add {
                        content: "hello".to_string(),
                    },
                )]),
                exec_approval_requirement: ExecApprovalRequirement::Skip {
                    bypass_sandbox: false,
                    proposed_execpolicy_amendment: None,
                },
                additional_permissions: None,
                permissions_preapproved: false,
            };
            let mut runtime = ApplyPatchRuntime::new();
            let retry_one_id = "retry-1".to_string();
            let retry_one = runtime
                .start_approval_async(
                    &req,
                    ApprovalCtx {
                        session: &session,
                        turn: &turn,
                        call_id: &retry_one_id,
                        guardian_review_id: None,
                        retry_reason: Some("retry without sandbox?".to_string()),
                        network_approval_context: None,
                    },
                )
                .await;
            let retry_two_id = "retry-2".to_string();
            let retry_two = runtime
                .start_approval_async(
                    &req,
                    ApprovalCtx {
                        session: &session,
                        turn: &turn,
                        call_id: &retry_two_id,
                        guardian_review_id: None,
                        retry_reason: Some("retry without sandbox?".to_string()),
                        network_approval_context: None,
                    },
                )
                .await;
            let ordinary_id = "ordinary".to_string();
            let ordinary = runtime
                .start_approval_async(
                    &req,
                    ApprovalCtx {
                        session: &session,
                        turn: &turn,
                        call_id: &ordinary_id,
                        guardian_review_id: None,
                        retry_reason: None,
                        network_approval_context: None,
                    },
                )
                .await;
            (retry_one, retry_two, ordinary)
        }
    });

    let first_event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
        .await
        .expect("first retry approval prompt")
        .expect("approval event channel");
    let EventMsg::ApplyPatchApprovalRequest(first_request) = first_event.msg else {
        panic!("expected first retry approval request");
    };
    assert_eq!(first_request.call_id, "retry-1");
    assert_eq!(
        first_request.reason.as_deref(),
        Some("retry without sandbox?")
    );
    session
        .notify_approval("retry-1", ReviewDecision::ApprovedForSession)
        .await;

    let ordinary_event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
        .await
        .expect("ordinary approval prompt")
        .expect("approval event channel");
    let EventMsg::ApplyPatchApprovalRequest(ordinary_request) = ordinary_event.msg else {
        panic!("expected ordinary approval request");
    };
    assert_eq!(ordinary_request.call_id, "ordinary");
    assert_eq!(ordinary_request.reason, None);
    session
        .notify_approval("ordinary", ReviewDecision::Denied)
        .await;

    let (retry_one, retry_two, ordinary) =
        tokio::time::timeout(std::time::Duration::from_secs(1), approvals)
            .await
            .expect("approval flow completes")
            .expect("approval task");
    assert_eq!(retry_one, ReviewDecision::ApprovedForSession);
    assert_eq!(retry_two, ReviewDecision::ApprovedForSession);
    assert_eq!(ordinary, ReviewDecision::Denied);
}

#[tokio::test]
async fn sandbox_cwd_uses_patch_action_cwd() {
    let runtime = ApplyPatchRuntime::new();
    let path = std::env::temp_dir()
        .join("apply-patch-runtime-sandbox-cwd.txt")
        .abs();
    let req = ApplyPatchRequest {
        turn_environment: test_turn_environment(codex_exec_server::LOCAL_ENVIRONMENT_ID),
        action: ApplyPatchAction::new_add_for_test(
            &PathUri::from_abs_path(&path),
            "hello".to_string(),
        ),
        file_paths: vec![PathUri::from_abs_path(&path)],
        changes: HashMap::new(),
        exec_approval_requirement: ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: None,
        permissions_preapproved: false,
    };

    assert_eq!(runtime.sandbox_cwd(&req), Some(&req.action.cwd));
}

#[tokio::test]
async fn file_system_sandbox_context_uses_active_attempt() {
    let path = std::env::temp_dir()
        .join("apply-patch-runtime-attempt.txt")
        .abs();
    let additional_permissions = AdditionalPermissionProfile {
        network: None,
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![path.clone()]),
            Some(Vec::new()),
        )),
    };
    let req = ApplyPatchRequest {
        turn_environment: test_turn_environment(codex_exec_server::LOCAL_ENVIRONMENT_ID),
        action: ApplyPatchAction::new_add_for_test(
            &PathUri::from_abs_path(&path),
            "hello".to_string(),
        ),
        file_paths: vec![PathUri::from_abs_path(&path)],
        changes: HashMap::new(),
        exec_approval_requirement: ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: Some(additional_permissions.clone()),
        permissions_preapproved: false,
    };
    let file_system_policy = FileSystemSandboxPolicy::default();
    let permissions = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );
    let sandbox_policy_cwd = PathUri::from_abs_path(&path);
    let attempt = SandboxAttempt {
        codex_home: &path,
        sandbox: SandboxType::WindowsRestrictedToken,
        sandbox_requested: true,
        permissions: &permissions,
        exec_server_permissions: &permissions,
        enforce_managed_network: false,
        sandbox_cwd: &sandbox_policy_cwd,
        workspace_roots: std::slice::from_ref(&path),
        windows_sandbox_level: WindowsSandboxLevel::RestrictedToken,
        windows_sandbox_private_desktop: true,
        network_denial_cancellation_token: None,
        network_proxy: None,
    };

    let sandbox = ApplyPatchRuntime::file_system_sandbox_context_for_attempt(&req, &attempt)
        .expect("sandbox context");

    let file_system_policy =
        effective_file_system_sandbox_policy(&file_system_policy, Some(&additional_permissions));
    let network_policy = effective_network_sandbox_policy(
        NetworkSandboxPolicy::Restricted,
        Some(&additional_permissions),
    );
    let expected_permissions =
        PermissionProfile::from_runtime_permissions(&file_system_policy, network_policy);
    let native_permissions: PermissionProfile = sandbox
        .permissions
        .clone()
        .try_into()
        .expect("native sandbox permissions");
    assert_eq!(native_permissions, expected_permissions);
    assert_eq!(
        sandbox.cwd,
        Some(codex_utils_path_uri::PathUri::from_abs_path(&path))
    );
    assert_eq!(
        sandbox.windows_sandbox_level,
        WindowsSandboxLevel::RestrictedToken
    );
    assert_eq!(sandbox.windows_sandbox_private_desktop, true);
}

#[tokio::test]
async fn no_sandbox_attempt_has_no_file_system_context() {
    let path = std::env::temp_dir()
        .join("apply-patch-runtime-none.txt")
        .abs();
    let req = ApplyPatchRequest {
        turn_environment: test_turn_environment(codex_exec_server::LOCAL_ENVIRONMENT_ID),
        action: ApplyPatchAction::new_add_for_test(
            &PathUri::from_abs_path(&path),
            "hello".to_string(),
        ),
        file_paths: vec![PathUri::from_abs_path(&path)],
        changes: HashMap::new(),
        exec_approval_requirement: ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: None,
        permissions_preapproved: false,
    };
    let permissions = PermissionProfile::Disabled;
    let sandbox_policy_cwd = PathUri::from_abs_path(&path);
    let attempt = SandboxAttempt {
        codex_home: &path,
        sandbox: SandboxType::None,
        sandbox_requested: false,
        permissions: &permissions,
        exec_server_permissions: &permissions,
        enforce_managed_network: false,
        sandbox_cwd: &sandbox_policy_cwd,
        workspace_roots: std::slice::from_ref(&path),
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: false,
        network_denial_cancellation_token: None,
        network_proxy: None,
    };

    assert_eq!(
        ApplyPatchRuntime::file_system_sandbox_context_for_attempt(&req, &attempt),
        None
    );
}
