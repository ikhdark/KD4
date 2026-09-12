use super::*;
use crate::guardian::GuardianRejection;
use crate::sandboxing::SandboxPermissions;
use codex_network_proxy::BlockedRequestArgs;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::GuardianAssessmentDecisionSource;
use core_test_support::PathBufExt;
use core_test_support::test_path_buf;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

// Supply an ongoing workload through normal task registration. Approval,
// transport, and cancellation behavior all use their production owners.
struct NetworkApprovalActiveTask;
impl crate::tasks::SessionTask for NetworkApprovalActiveTask {
    fn kind(&self) -> crate::state::TaskKind {
        crate::state::TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.network_disconnect_test"
    }

    fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<crate::session::turn_context::TurnContext>,
        _input: Vec<crate::session::TurnInput>,
        cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, crate::tasks::SessionTaskResult> {
        Box::pin(async move {
            cancellation_token.cancelled().await;
            Err(codex_protocol::error::CodexErr::TurnAborted)
        })
    }
}

async fn open_network_approval_http_request(
    proxy_address: std::net::SocketAddr,
    target: &str,
) -> anyhow::Result<TcpStream> {
    let mut socket = TcpStream::connect(proxy_address).await?;
    socket
        .write_all(
            format!(
                "GET http://{target}/approval-disconnect HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    Ok(socket)
}

async fn next_network_approval_event(
    events: &async_channel::Receiver<Event>,
) -> codex_protocol::protocol::ExecApprovalRequestEvent {
    timeout(Duration::from_secs(5), async {
        loop {
            if let EventMsg::ExecApprovalRequest(approval) = events.recv().await.unwrap().msg {
                break approval;
            }
        }
    })
    .await
    .expect("real HTTP request should publish a command approval")
}

#[tokio::test]
async fn http_disconnect_denies_follower_and_requires_fresh_approval() -> anyhow::Result<()> {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/approval-disconnect"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("fresh-approval-only"))
        .expect(1)
        .mount(&upstream)
        .await;
    let (session, mut turn, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let context = Arc::get_mut(&mut turn).expect("fixture has one turn context owner");
    context.permission_profile = PermissionProfile::read_only();
    context.approval_policy = codex_config::Constrained::allow_any(AskForApproval::OnRequest);
    Arc::make_mut(&mut context.config).approvals_reviewer =
        codex_config::types::ApprovalsReviewer::User;
    let service = Arc::clone(&session.services.network_approval);
    let spec = crate::config::NetworkProxySpec::from_config_and_constraints(
        codex_network_proxy::NetworkProxyConfig {
            enabled: true,
            proxy_url: "http://127.0.0.1:0".to_string(),
            enable_socks5: false,
            allow_local_binding: true,
            allow_upstream_proxy: false,
            ..Default::default()
        },
        None,
        &turn.permission_profile(),
    )?;
    let proxy_owner = spec
        .start_proxy(
            turn.config.codex_home.as_path(),
            &turn.permission_profile(),
            Some(build_network_policy_decider(
                Arc::clone(&service),
                Arc::new(RwLock::new(Arc::downgrade(&session))),
            )),
            None,
            true,
            codex_network_proxy::NetworkProxyAuditMetadata::default(),
        )
        .await?;
    let proxy_address = proxy_owner.proxy().http_addr();
    session
        .spawn_task(Arc::clone(&turn), Vec::new(), NetworkApprovalActiveTask)
        .await;

    let target = upstream.address().to_string();
    let owner = open_network_approval_http_request(proxy_address, &target).await?;
    let first_approval = next_network_approval_event(&events).await;
    assert_eq!(first_approval.turn_id, turn.sub_id);
    let context = first_approval.network_approval_context.as_ref().unwrap();
    assert_eq!(context.host, "127.0.0.1");
    assert_eq!(context.protocol, NetworkApprovalProtocol::Http);
    let key = {
        let pending = service.pending_host_approvals.lock().await;
        assert_eq!(pending.len(), 1);
        pending.keys().next().unwrap().clone()
    };
    assert_eq!(key.port, upstream.address().port());
    let mut follower = open_network_approval_http_request(proxy_address, &target).await?;
    timeout(Duration::from_secs(5), async {
        loop {
            // The map and owner hold two references; the third establishes
            // that the real second HTTP handler is waiting on this same entry.
            if service
                .pending_host_approvals
                .lock()
                .await
                .get(&key)
                .is_some_and(|pending| Arc::strong_count(pending) >= 3)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("same-host HTTP follower should join the pending approval");
    assert!(upstream.received_requests().await.unwrap().is_empty());

    // The stimulus is actual client EOF, not aborting a decider/handler task.
    drop(owner);
    let mut denied = Vec::new();
    timeout(Duration::from_secs(5), follower.read_to_end(&mut denied)).await??;
    let denied = String::from_utf8(denied)?;
    assert!(denied.starts_with("HTTP/1.1 403"), "{denied}");
    assert!(denied.contains("\"status\":\"blocked\""), "{denied}");
    assert!(denied.contains("\"reason\":\"not_allowed\""), "{denied}");
    timeout(Duration::from_secs(5), async {
        while service
            .pending_host_approvals
            .lock()
            .await
            .contains_key(&key)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnected owner must remove its pending identity");
    assert!(!service.session_denied_hosts.lock().await.contains(&key));
    assert!(!service.session_approved_hosts.lock().await.contains(&key));

    let mut fresh = open_network_approval_http_request(proxy_address, &target).await?;
    let fresh_approval = next_network_approval_event(&events).await;
    assert_ne!(fresh_approval.call_id, first_approval.call_id);
    assert_eq!(
        fresh_approval.network_approval_context,
        first_approval.network_approval_context
    );
    session
        .notify_approval(&first_approval.call_id, ReviewDecision::ApprovedForSession)
        .await;
    let mut premature_byte = [0_u8; 1];
    assert!(
        timeout(Duration::from_millis(100), fresh.read(&mut premature_byte))
            .await
            .is_err(),
        "late approval for the disconnected owner must not resolve the new request"
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert!(!service.session_approved_hosts.lock().await.contains(&key));
    session
        .notify_approval(&fresh_approval.call_id, ReviewDecision::Approved)
        .await;
    let mut allowed = Vec::new();
    timeout(Duration::from_secs(5), fresh.read_to_end(&mut allowed)).await??;
    let allowed = String::from_utf8(allowed)?;
    assert!(allowed.starts_with("HTTP/1.1 200"), "{allowed}");
    assert!(allowed.contains("fresh-approval-only"), "{allowed}");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert!(
        !service
            .pending_host_approvals
            .lock()
            .await
            .contains_key(&key)
    );
    assert!(session.active_turn.lock().await.is_some());
    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
    drop(proxy_owner);
    Ok(())
}

#[tokio::test]
async fn pending_approvals_are_deduped_per_host_protocol_and_port() {
    let service = NetworkApprovalService::default();
    let key = HostApprovalKey {
        environment_id: "local".to_string(),
        approval_scope_id: "local-scope".to_string(),
        host: "example.com".to_string(),
        protocol: "http",
        port: 443,
    };

    let (first, first_is_owner) = service.get_or_create_pending_approval(key.clone()).await;
    let (second, second_is_owner) = service.get_or_create_pending_approval(key).await;

    assert!(first_is_owner);
    assert!(!second_is_owner);
    assert!(Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn ownerless_guardian_denial_consumes_its_rejection_rationale() {
    let (session, _turn) = crate::session::tests::make_session_and_context().await;
    let review_id = "ownerless-network-denial";
    session.services.guardian_rejections.lock().await.insert(
        review_id.to_string(),
        GuardianRejection {
            rationale: "blocked by the test reviewer".to_string(),
            source: GuardianAssessmentDecisionSource::Agent,
        },
    );

    assert!(
        guardian_denial_outcome(&session, review_id, /*has_owner*/ false)
            .await
            .is_none()
    );
    assert!(
        !session
            .services
            .guardian_rejections
            .lock()
            .await
            .contains_key(review_id)
    );
}

#[tokio::test]
async fn pending_approvals_do_not_dedupe_across_ports() {
    let service = NetworkApprovalService::default();
    let first_key = HostApprovalKey {
        environment_id: "local".to_string(),
        approval_scope_id: "local-scope".to_string(),
        host: "example.com".to_string(),
        protocol: "https",
        port: 443,
    };
    let second_key = HostApprovalKey {
        environment_id: "local".to_string(),
        approval_scope_id: "local-scope".to_string(),
        host: "example.com".to_string(),
        protocol: "https",
        port: 8443,
    };

    let (first, first_is_owner) = service.get_or_create_pending_approval(first_key).await;
    let (second, second_is_owner) = service.get_or_create_pending_approval(second_key).await;

    assert!(first_is_owner);
    assert!(second_is_owner);
    assert!(!Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn pending_approvals_do_not_dedupe_across_environments() {
    let service = NetworkApprovalService::default();
    let first_key = HostApprovalKey {
        environment_id: "local".to_string(),
        approval_scope_id: "local-scope".to_string(),
        host: "example.com".to_string(),
        protocol: "https",
        port: 443,
    };
    let second_key = HostApprovalKey {
        environment_id: "remote".to_string(),
        ..first_key.clone()
    };

    let (first, first_is_owner) = service.get_or_create_pending_approval(first_key).await;
    let (second, second_is_owner) = service.get_or_create_pending_approval(second_key).await;

    assert!(first_is_owner);
    assert!(second_is_owner);
    assert!(!Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn session_approved_hosts_are_scoped_by_environment() {
    let service = NetworkApprovalService::default();
    let local_key = HostApprovalKey {
        environment_id: "local".to_string(),
        approval_scope_id: "local-scope".to_string(),
        host: "example.com".to_string(),
        protocol: "https",
        port: 443,
    };
    let remote_key = HostApprovalKey {
        environment_id: "remote".to_string(),
        ..local_key.clone()
    };
    service
        .session_approved_hosts
        .lock()
        .await
        .insert(local_key);

    assert!(
        !service
            .session_approved_hosts
            .lock()
            .await
            .contains(&remote_key)
    );
}

#[tokio::test]
async fn session_approved_hosts_are_scoped_by_environment_incarnation() {
    let service = NetworkApprovalService::default();
    let first_key = HostApprovalKey {
        environment_id: "remote".to_string(),
        approval_scope_id: "remote-scope-1".to_string(),
        host: "example.com".to_string(),
        protocol: "https",
        port: 443,
    };
    let replacement_key = HostApprovalKey {
        approval_scope_id: "remote-scope-2".to_string(),
        ..first_key.clone()
    };
    service
        .session_approved_hosts
        .lock()
        .await
        .insert(first_key);

    assert!(
        !service
            .session_approved_hosts
            .lock()
            .await
            .contains(&replacement_key)
    );
}

#[tokio::test]
async fn session_approved_hosts_preserve_protocol_and_port_scope() {
    let source = NetworkApprovalService::default();
    {
        let mut approved_hosts = source.session_approved_hosts.lock().await;
        approved_hosts.extend([
            HostApprovalKey {
                environment_id: "local".to_string(),
                approval_scope_id: "local-scope".to_string(),
                host: "example.com".to_string(),
                protocol: "https",
                port: 443,
            },
            HostApprovalKey {
                environment_id: "local".to_string(),
                approval_scope_id: "local-scope".to_string(),
                host: "example.com".to_string(),
                protocol: "https",
                port: 8443,
            },
            HostApprovalKey {
                environment_id: "local".to_string(),
                approval_scope_id: "local-scope".to_string(),
                host: "example.com".to_string(),
                protocol: "http",
                port: 80,
            },
        ]);
    }

    let seeded = NetworkApprovalService::default();
    source.sync_session_approved_hosts_to(&seeded).await;

    let mut copied = seeded
        .session_approved_hosts
        .lock()
        .await
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    copied.sort_by(|a, b| {
        (&a.environment_id, &a.host, a.protocol, a.port).cmp(&(
            &b.environment_id,
            &b.host,
            b.protocol,
            b.port,
        ))
    });

    assert_eq!(
        copied,
        vec![
            HostApprovalKey {
                environment_id: "local".to_string(),
                approval_scope_id: "local-scope".to_string(),
                host: "example.com".to_string(),
                protocol: "http",
                port: 80,
            },
            HostApprovalKey {
                environment_id: "local".to_string(),
                approval_scope_id: "local-scope".to_string(),
                host: "example.com".to_string(),
                protocol: "https",
                port: 443,
            },
            HostApprovalKey {
                environment_id: "local".to_string(),
                approval_scope_id: "local-scope".to_string(),
                host: "example.com".to_string(),
                protocol: "https",
                port: 8443,
            },
        ]
    );
}

#[tokio::test]
async fn sync_session_approved_hosts_to_replaces_existing_target_hosts() {
    let source = NetworkApprovalService::default();
    {
        let mut approved_hosts = source.session_approved_hosts.lock().await;
        approved_hosts.insert(HostApprovalKey {
            environment_id: "local".to_string(),
            approval_scope_id: "local-scope".to_string(),
            host: "source.example.com".to_string(),
            protocol: "https",
            port: 443,
        });
    }

    let target = NetworkApprovalService::default();
    {
        let mut approved_hosts = target.session_approved_hosts.lock().await;
        approved_hosts.insert(HostApprovalKey {
            environment_id: "local".to_string(),
            approval_scope_id: "local-scope".to_string(),
            host: "stale.example.com".to_string(),
            protocol: "https",
            port: 8443,
        });
    }

    source.sync_session_approved_hosts_to(&target).await;

    let copied = target
        .session_approved_hosts
        .lock()
        .await
        .iter()
        .cloned()
        .collect::<Vec<_>>();

    assert_eq!(
        copied,
        vec![HostApprovalKey {
            environment_id: "local".to_string(),
            approval_scope_id: "local-scope".to_string(),
            host: "source.example.com".to_string(),
            protocol: "https",
            port: 443,
        }]
    );
}

#[tokio::test]
async fn pending_waiters_receive_owner_decision() {
    let pending = Arc::new(PendingHostApproval::new());

    let waiter = {
        let pending = Arc::clone(&pending);
        tokio::spawn(async move { pending.wait_for_decision().await })
    };

    pending
        .set_decision(PendingApprovalDecision::AllowOnce)
        .await;

    let decision = waiter.await.expect("waiter should complete");
    assert_eq!(decision, PendingApprovalDecision::AllowOnce);
}

#[test]
fn allow_once_and_allow_for_session_both_allow_network() {
    assert_eq!(
        PendingApprovalDecision::AllowOnce.to_network_decision(),
        NetworkDecision::Allow
    );
    assert_eq!(
        PendingApprovalDecision::AllowForSession.to_network_decision(),
        NetworkDecision::Allow
    );
}

#[test]
fn only_never_policy_disables_network_approval_flow() {
    assert!(!allows_network_approval_flow(AskForApproval::Never));
    assert!(allows_network_approval_flow(AskForApproval::OnRequest));
    assert!(allows_network_approval_flow(AskForApproval::UnlessTrusted));
}

#[test]
fn network_approval_flow_is_limited_to_restricted_sandbox_modes() {
    assert!(permission_profile_allows_network_approval_flow(
        &PermissionProfile::read_only()
    ));
    assert!(permission_profile_allows_network_approval_flow(
        &PermissionProfile::workspace_write()
    ));
    assert!(!permission_profile_allows_network_approval_flow(
        &PermissionProfile::Disabled
    ));
    assert!(!permission_profile_allows_network_approval_flow(
        &PermissionProfile::External {
            network: NetworkSandboxPolicy::Restricted,
        }
    ));
}

fn denied_blocked_request(host: &str) -> BlockedRequest {
    BlockedRequest::new(BlockedRequestArgs {
        host: host.to_string(),
        reason: "not_allowed".to_string(),
        client: None,
        method: None,
        mode: None,
        protocol: "http".to_string(),
        decision: Some("deny".to_string()),
        source: Some("decider".to_string()),
        port: Some(80),
    })
}

fn denied_blocked_request_for_execution(host: &str, execution_id: &str) -> BlockedRequest {
    let mut blocked = denied_blocked_request(host);
    blocked.execution_id = Some(execution_id.to_string());
    blocked
}

async fn register_call_with_default_shell_trigger(
    service: &NetworkApprovalService,
    registration_id: &str,
) -> CancellationToken {
    let cancellation_token = CancellationToken::new();
    service
        .register_call(
            registration_id.to_string(),
            "turn-1".to_string(),
            GuardianNetworkAccessTrigger {
                call_id: "call-1".to_string(),
                tool_name: "shell_command".to_string(),
                command: vec!["curl".to_string(), "https://example.com".to_string()],
                cwd: test_path_buf("/tmp").abs().into(),
                sandbox_permissions: SandboxPermissions::UseDefault,
                additional_permissions: None,
                justification: None,
                tty: None,
            },
            "curl https://example.com".to_string(),
            "local".to_string(),
            "local-scope".to_string(),
            cancellation_token.clone(),
        )
        .await;
    cancellation_token
}

#[tokio::test]
async fn active_call_preserves_triggering_command_context() {
    let service = NetworkApprovalService::default();
    let expected = GuardianNetworkAccessTrigger {
        call_id: "call-1".to_string(),
        tool_name: "shell_command".to_string(),
        command: vec!["curl".to_string(), "https://example.com".to_string()],
        cwd: test_path_buf("/repo").abs().into(),
        sandbox_permissions: SandboxPermissions::UseDefault,
        additional_permissions: None,
        justification: Some("fetch release metadata".to_string()),
        tty: None,
    };

    service
        .register_call(
            "registration-1".to_string(),
            "turn-1".to_string(),
            expected.clone(),
            "curl https://example.com".to_string(),
            "remote".to_string(),
            "remote-scope".to_string(),
            CancellationToken::new(),
        )
        .await;

    let call = service
        .resolve_single_active_call()
        .await
        .expect("single active call should resolve");

    assert_eq!(&call.trigger, &expected);
    assert_eq!(call.command, "curl https://example.com");
    assert_eq!(call.environment_id, "remote");
    assert_eq!(call.approval_scope_id, "remote-scope");
}

#[tokio::test]
async fn multiple_active_calls_are_ambiguous_even_in_the_same_environment() {
    let service = NetworkApprovalService::default();
    register_call_with_default_shell_trigger(&service, "registration-1").await;
    register_call_with_default_shell_trigger(&service, "registration-2").await;

    match service.resolve_active_call_attribution().await {
        ActiveNetworkApprovalAttribution::Ambiguous => {}
        ActiveNetworkApprovalAttribution::None | ActiveNetworkApprovalAttribution::Single(_) => {
            panic!("multiple active calls should be ambiguous")
        }
    }
}

#[tokio::test]
async fn record_blocked_request_sets_policy_outcome_for_owner_call() {
    let service = NetworkApprovalService::default();
    let cancellation_token =
        register_call_with_default_shell_trigger(&service, "registration-1").await;

    service
        .record_blocked_request(denied_blocked_request("example.com"))
        .await;

    assert!(cancellation_token.is_cancelled());
    assert_eq!(
            service.take_call_outcome("registration-1").await,
            Some(NetworkApprovalOutcome::DeniedByPolicy(
                "Network access to \"example.com\" was blocked: domain is not on the allowlist for the current sandbox mode.".to_string()
            ))
        );
}

#[tokio::test]
async fn blocked_request_policy_does_not_override_user_denial_outcome() {
    let service = NetworkApprovalService::default();
    register_call_with_default_shell_trigger(&service, "registration-1").await;

    service
        .record_call_outcome("registration-1", NetworkApprovalOutcome::DeniedByUser)
        .await;
    service
        .record_blocked_request(denied_blocked_request("example.com"))
        .await;

    assert_eq!(
        service.take_call_outcome("registration-1").await,
        Some(NetworkApprovalOutcome::DeniedByUser)
    );
}

#[tokio::test]
async fn finish_call_returns_denial_and_unregisters_active_call() {
    let service = NetworkApprovalService::default();
    register_call_with_default_shell_trigger(&service, "registration-1").await;

    service
        .record_call_outcome(
            "registration-1",
            NetworkApprovalOutcome::DeniedByPolicy("network denied".to_string()),
        )
        .await;

    let err = service
        .finish_call("registration-1")
        .await
        .expect_err("denial should be returned");

    assert!(matches!(err, ToolError::Denied(message) if message == "network denied"));
    assert!(service.resolve_single_active_call().await.is_none());
    assert_eq!(service.take_call_outcome("registration-1").await, None);
}

#[tokio::test]
async fn deferred_finish_reuses_denial_result_after_first_consumer() {
    let service = Arc::new(NetworkApprovalService::default());
    let cancellation_token =
        register_call_with_default_shell_trigger(&service, "registration-1").await;
    let deferred = DeferredNetworkApproval {
        registration: Arc::new(NetworkApprovalRegistration::new(
            "registration-1".to_string(),
            Arc::clone(&service),
        )),
        cancellation_token,
        finish_outcome: Arc::new(OnceCell::new()),
        _execution_proxy: None,
    };
    service
        .record_call_outcome(
            "registration-1",
            NetworkApprovalOutcome::DeniedByPolicy("network denied".to_string()),
        )
        .await;

    let first = deferred
        .finish()
        .await
        .expect_err("first consumer should see denial");
    let second = deferred
        .finish()
        .await
        .expect_err("second consumer should reuse denial");

    assert!(matches!(first, ToolError::Denied(message) if message == "network denied"));
    assert!(matches!(second, ToolError::Denied(message) if message == "network denied"));
}

#[tokio::test]
async fn record_call_outcome_ignores_inactive_call() {
    let service = NetworkApprovalService::default();
    let cancellation_token =
        register_call_with_default_shell_trigger(&service, "registration-1").await;
    service.unregister_call("registration-1").await;

    service
        .record_call_outcome(
            "registration-1",
            NetworkApprovalOutcome::DeniedByPolicy("network denied".to_string()),
        )
        .await;

    assert!(!cancellation_token.is_cancelled());
    assert_eq!(service.take_call_outcome("registration-1").await, None);
}

#[tokio::test]
async fn ambiguous_unattributed_blocked_request_marks_and_cancels_every_candidate() {
    let service = NetworkApprovalService::default();
    let first = register_call_with_default_shell_trigger(&service, "registration-1").await;
    let second = register_call_with_default_shell_trigger(&service, "registration-2").await;

    service
        .record_blocked_request(denied_blocked_request("example.com"))
        .await;

    assert!(first.is_cancelled());
    assert!(second.is_cancelled());
    for registration_id in ["registration-1", "registration-2"] {
        assert!(matches!(
            service.take_call_outcome(registration_id).await,
            Some(NetworkApprovalOutcome::DeniedByPolicy(message))
                if message.contains("attribution was ambiguous across 2 active tool calls")
        ));
    }
}

#[test]
fn dropped_network_registration_is_unregistered_without_explicit_finish() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (session, proxy_owner, deferred, immediate) = runtime.block_on(async {
        let (session, turn, _) = crate::session::tests::make_session_and_context_with_rx().await;
        let proxy_spec = crate::config::NetworkProxySpec::from_config_and_constraints(
            codex_network_proxy::NetworkProxyConfig {
                enabled: true,
                proxy_url: "http://127.0.0.1:0".to_string(),
                enable_socks5: false,
                allow_local_binding: true,
                allow_upstream_proxy: false,
                ..Default::default()
            },
            None,
            &turn.permission_profile(),
        )?;
        let proxy_owner = proxy_spec
            .start_proxy(
                turn.config.codex_home.as_path(),
                &turn.permission_profile(),
                None,
                None,
                true,
                codex_network_proxy::NetworkProxyAuditMetadata::default(),
            )
            .await?;
        let spec = |mode| NetworkApprovalSpec {
            network: Some(proxy_owner.proxy().clone()),
            mode,
            trigger: GuardianNetworkAccessTrigger {
                call_id: "drop-registration".to_string(),
                tool_name: "shell_command".to_string(),
                command: vec!["curl".to_string(), "https://example.com".to_string()],
                cwd: turn.cwd().clone().into(),
                sandbox_permissions: SandboxPermissions::UseDefault,
                additional_permissions: None,
                justification: None,
                tty: None,
            },
            command: "curl https://example.com".to_string(),
            environment_id: "local".to_string(),
            approval_scope_id: "local-scope".to_string(),
        };
        let deferred = begin_network_approval(
            &session,
            &turn.sub_id,
            true,
            Some(spec(NetworkApprovalMode::Deferred)),
        )
        .await
        .expect("normal deferred registration succeeds")
        .expect("normal registration returns its owner")
        .into_deferred()
        .expect("deferred mode transfers its owner");
        let immediate = begin_network_approval(
            &session,
            &turn.sub_id,
            true,
            Some(spec(NetworkApprovalMode::Immediate)),
        )
        .await
        .expect("normal immediate registration succeeds")
        .expect("independent immediate registration");
        Ok::<_, anyhow::Error>((session, proxy_owner, deferred, immediate))
    })?;
    let service = &session.services.network_approval;
    let deferred_id = deferred.registration_id().to_string();
    let immediate_id = immediate
        .registration
        .as_ref()
        .unwrap()
        .registration_id()
        .to_string();
    let survivor_token = immediate.cancellation_token();
    runtime.block_on(
        service.record_blocked_request(denied_blocked_request_for_execution(
            "example.com",
            &deferred_id,
        )),
    );
    assert!(deferred.is_cancelled());
    assert!(!survivor_token.is_cancelled());
    let final_owner = deferred.clone();
    assert!(tokio::runtime::Handle::try_current().is_err());
    drop(deferred);
    {
        let calls = service.calls.lock().unwrap();
        assert_eq!(calls.active_calls.len(), 2);
        assert!(calls.call_outcomes.contains_key(&deferred_id));
    }
    drop(final_owner);
    {
        let calls = service.calls.lock().unwrap();
        assert_eq!(
            calls.active_calls.keys().collect::<Vec<_>>(),
            vec![&immediate_id]
        );
        assert!(calls.call_outcomes.is_empty());
    }
    runtime.block_on(
        service.record_blocked_request(denied_blocked_request_for_execution(
            "example.com",
            &deferred_id,
        )),
    );
    assert!(
        !survivor_token.is_cancelled(),
        "late attribution cannot cancel a different owner"
    );
    assert!(service.calls.lock().unwrap().call_outcomes.is_empty());
    drop(runtime);
    drop(immediate);
    let calls = service.calls.lock().unwrap();
    assert!(
        calls.active_calls.is_empty(),
        "last-owner cleanup survives runtime shutdown"
    );
    assert!(calls.call_outcomes.is_empty());
    drop(calls);
    drop(proxy_owner);
    Ok(())
}

#[tokio::test]
async fn attributed_blocked_request_targets_one_of_multiple_active_calls() {
    let service = NetworkApprovalService::default();
    let first = register_call_with_default_shell_trigger(&service, "registration-1").await;
    let second = register_call_with_default_shell_trigger(&service, "registration-2").await;

    service
        .record_blocked_request(denied_blocked_request_for_execution(
            "example.com",
            "registration-2",
        ))
        .await;

    assert!(!first.is_cancelled());
    assert!(second.is_cancelled());
    assert_eq!(service.take_call_outcome("registration-1").await, None);
    assert_eq!(
        service.take_call_outcome("registration-2").await,
        Some(NetworkApprovalOutcome::DeniedByPolicy(
            "Network access to \"example.com\" was blocked: domain is not on the allowlist for the current sandbox mode.".to_string()
        ))
    );
}

#[tokio::test]
async fn http_network_approval_preserves_foreign_environment_cwd_uri() -> anyhow::Result<()> {
    use crate::tools::sandboxing::ToolRuntime;
    use codex_utils_path_uri::PathUri;

    let foreign_cwd = PathUri::parse(if cfg!(windows) {
        "file:///home/remote/network-project"
    } else {
        "file:///C:/remote/network-project"
    })?;
    assert!(foreign_cwd.to_abs_path().is_err());
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/approval-disconnect"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("foreign-cwd-approved"))
        .expect(1)
        .mount(&upstream)
        .await;
    let (session, mut turn, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let host_cwd = turn.cwd().clone();
    let context = Arc::get_mut(&mut turn).expect("fixture has one turn context owner");
    context.permission_profile = PermissionProfile::read_only();
    context.approval_policy = codex_config::Constrained::allow_any(AskForApproval::OnRequest);
    Arc::make_mut(&mut context.config).approvals_reviewer =
        codex_config::types::ApprovalsReviewer::User;
    let shell = context.environments.primary().unwrap().shell.clone();
    context.environments.turn_environments[0] = crate::session::turn_context::TurnEnvironment::new(
        "remote-network-approval".to_string(),
        Arc::new(codex_exec_server::Environment::default_for_tests()),
        foreign_cwd.clone(),
        shell,
    );
    let service = Arc::clone(&session.services.network_approval);
    let spec = crate::config::NetworkProxySpec::from_config_and_constraints(
        codex_network_proxy::NetworkProxyConfig {
            enabled: true,
            proxy_url: "http://127.0.0.1:0".to_string(),
            enable_socks5: false,
            allow_local_binding: true,
            allow_upstream_proxy: false,
            ..Default::default()
        },
        None,
        &turn.permission_profile(),
    )?;
    let proxy_owner = spec
        .start_proxy(
            turn.config.codex_home.as_path(),
            &turn.permission_profile(),
            Some(build_network_policy_decider(
                Arc::clone(&service),
                Arc::new(RwLock::new(Arc::downgrade(&session))),
            )),
            None,
            true,
            codex_network_proxy::NetworkProxyAuditMetadata::default(),
        )
        .await?;
    let command = vec!["curl".to_string(), upstream.uri()];
    let request = crate::tools::runtimes::unified_exec::UnifiedExecRequest {
        command: command.clone(),
        command_for_approval: command.clone(),
        normalization_cwd: None,
        approved_powershell_direct_argv: None,
        raw_output_artifact: crate::tools::command_output_artifact::RawOutputArtifact::Failed {
            id: None,
            message: "network registration fixture does not launch a process".to_string(),
            owned_path: None,
            bytes: 0,
        },
        shell_type: crate::shell::ShellType::Sh,
        hook_command: format!("curl {}", upstream.uri()),
        process_id: 1000,
        cwd: foreign_cwd.clone(),
        sandbox_cwd: foreign_cwd.clone(),
        turn_environment: turn.environments.primary().unwrap().clone(),
        env: std::collections::HashMap::new(),
        exec_server_env_config: None,
        explicit_env_overrides: std::collections::HashMap::new(),
        network: Some(proxy_owner.proxy().clone()),
        tty: false,
        sandbox_permissions: SandboxPermissions::UseDefault,
        additional_permissions: None,
        additional_permissions_uri: None,
        justification: None,
        exec_approval_requirement: crate::tools::sandboxing::ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
        validation_launch: None,
        known_delta_hit: None,
    };
    let runtime = crate::tools::runtimes::unified_exec::UnifiedExecRuntime::new(
        &session.services.unified_exec_manager,
    );
    let context = crate::tools::sandboxing::ToolCtx {
        session: session.clone(),
        turn: turn.clone(),
        call_id: "foreign-network-exec".to_string(),
        tool_name: codex_tools::ToolName::plain("exec_command"),
    };
    let registered = begin_network_approval(
        &session,
        &turn.sub_id,
        true,
        runtime.network_approval_spec(&request, &context),
    )
    .await
    .expect("normal runtime network registration")
    .expect("foreign cwd must retain its triggering execution owner")
    .into_deferred()
    .expect("unified execution uses deferred approval ownership");
    let call = service
        .resolve_single_active_call()
        .await
        .expect("registered execution context");
    assert_eq!(call.trigger.call_id, "foreign-network-exec");
    assert_eq!(call.trigger.command, command);
    assert_eq!(call.trigger.cwd, foreign_cwd);
    assert_eq!(
        serde_json::to_value(&call.trigger)?["cwd"],
        serde_json::json!(foreign_cwd)
    );
    drop(call);
    session
        .spawn_task(Arc::clone(&turn), Vec::new(), NetworkApprovalActiveTask)
        .await;
    let target = upstream.address().to_string();
    let mut client =
        open_network_approval_http_request(proxy_owner.proxy().http_addr(), &target).await?;
    let approval = next_network_approval_event(&events).await;
    assert_eq!(approval.turn_id, turn.sub_id);
    assert_eq!(
        approval.environment_id.as_deref(),
        Some("remote-network-approval")
    );
    assert_eq!(approval.cwd_uri, Some(foreign_cwd));
    // Legacy native-only clients retain the host fallback; the URI is authoritative.
    assert_eq!(approval.cwd, host_cwd);
    assert_eq!(
        approval.network_approval_context,
        Some(NetworkApprovalContext {
            host: "127.0.0.1".to_string(),
            protocol: NetworkApprovalProtocol::Http,
        })
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
    session
        .notify_approval(&approval.call_id, ReviewDecision::Approved)
        .await;
    let mut response = Vec::new();
    timeout(Duration::from_secs(5), client.read_to_end(&mut response)).await??;
    let response = String::from_utf8(response)?;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("foreign-cwd-approved"), "{response}");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert!(service.pending_host_approvals.lock().await.is_empty());
    finish_deferred_network_approval(&session, Some(registered))
        .await
        .expect("completed network request releases its normal registration");
    assert!(service.calls.lock().unwrap().active_calls.is_empty());
    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
    drop(proxy_owner);
    Ok(())
}
