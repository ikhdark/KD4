use super::*;
use crate::unified_exec::ADAPTIVE_DIAGNOSTIC_OUTPUT_TOKENS;
use crate::unified_exec::DEFAULT_FAILURE_OUTPUT_TOKENS;
use crate::unified_exec::DEFAULT_SUCCESS_OUTPUT_TOKENS;
use crate::unified_exec::OutputBudgetClass;
use crate::unified_exec::async_watcher::resolve_aggregated_output;
use crate::unified_exec::clamp_yield_time;
use crate::unified_exec::clamp_yield_time_for_readiness;
use crate::unified_exec::resolve_adaptive_max_tokens;
use codex_network_proxy::ManagedNetworkSandboxContext;
use pretty_assertions::assert_eq;
use tokio::time::Duration;
use tokio::time::Instant;

#[test]
fn adaptive_output_budget_uses_relaxed_defaults_and_diagnostic_escalation() {
    assert_eq!(DEFAULT_SUCCESS_OUTPUT_TOKENS, 8_000);
    assert_eq!(DEFAULT_FAILURE_OUTPUT_TOKENS, 10_000);
    assert_eq!(
        resolve_adaptive_max_tokens(None, OutputBudgetClass::Success, Some("echo ok"), "ok"),
        DEFAULT_SUCCESS_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve_adaptive_max_tokens(
            None,
            OutputBudgetClass::FailureOrTimeout,
            Some("custom-command"),
            "failed"
        ),
        DEFAULT_FAILURE_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve_adaptive_max_tokens(
            None,
            OutputBudgetClass::Success,
            Some("cargo nextest run -p codex-core"),
            "tests passed"
        ),
        ADAPTIVE_DIAGNOSTIC_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve_adaptive_max_tokens(
            None,
            OutputBudgetClass::FailureOrTimeout,
            None,
            "Traceback (most recent call last):"
        ),
        ADAPTIVE_DIAGNOSTIC_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve_adaptive_max_tokens(
            Some(1_234),
            OutputBudgetClass::FailureOrTimeout,
            Some("cargo test"),
            "test result: FAILED"
        ),
        1_234
    );
}

#[test]
fn unified_exec_env_injects_defaults() {
    let env = apply_unified_exec_env(HashMap::new());
    let expected = HashMap::from([
        ("NO_COLOR".to_string(), "1".to_string()),
        ("TERM".to_string(), "dumb".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("LC_CTYPE".to_string(), "C.UTF-8".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ("COLORTERM".to_string(), String::new()),
        ("PAGER".to_string(), "cat".to_string()),
        ("GIT_PAGER".to_string(), "cat".to_string()),
        ("GH_PAGER".to_string(), "cat".to_string()),
        ("CODEX_CI".to_string(), "1".to_string()),
    ]);

    assert_eq!(env, expected);
}

#[test]
fn unified_exec_env_overrides_existing_values() {
    let mut base = HashMap::new();
    base.insert("NO_COLOR".to_string(), "0".to_string());
    base.insert("PATH".to_string(), "/usr/bin".to_string());

    let env = apply_unified_exec_env(base);

    assert_eq!(env.get("NO_COLOR"), Some(&"1".to_string()));
    assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
}

#[tokio::test]
async fn lag_survives_drain_for_finalization_without_duplicate_interim_reports() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(256)));
    {
        let mut guard = output_buffer.lock().await;
        guard.push_chunk(b"PARTIAL_OUTPUT".to_vec());
        guard.record_lagged_chunks(4);
    }
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(true));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    cancellation_token.cancel();

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        /*pause_state*/ None,
        Instant::now() + Duration::from_millis(50),
        CollectionMode::UntilDeadline,
    )
    .await;
    let collected = String::from_utf8(collected).expect("collected output is UTF-8");
    assert!(collected.contains("PARTIAL_OUTPUT"));
    assert_eq!(
        collected
            .matches("streaming receiver lagged by 4 chunk(s)")
            .count(),
        1
    );

    let second_drain = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        /*pause_state*/ None,
        Instant::now() + Duration::from_millis(50),
        CollectionMode::UntilDeadline,
    )
    .await;
    assert!(second_drain.is_empty());

    let final_output = resolve_aggregated_output(&output_buffer, "FINAL_OUTPUT".to_string()).await;
    assert!(final_output.contains("FINAL_OUTPUT"));
    assert_eq!(
        final_output
            .matches("streaming receiver lagged by 4 chunk(s)")
            .count(),
        1
    );
    assert_eq!(output_buffer.lock().await.lagged_chunks(), 4);
}

#[tokio::test]
async fn capacity_omission_is_reported_once_per_drain_and_preserved_for_finalization() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(256)));
    output_buffer.lock().await.push_chunk(vec![b'a'; 512]);
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(true));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    cancellation_token.cancel();

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        /*pause_state*/ None,
        Instant::now() + Duration::from_millis(50),
        CollectionMode::UntilDeadline,
    )
    .await;
    let collected = String::from_utf8(collected).expect("collected output is UTF-8");
    assert_eq!(
        collected
            .matches("256 byte(s) omitted from the middle")
            .count(),
        1
    );
    assert!(!collected.contains("streaming receiver lagged"));

    let second_drain = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        /*pause_state*/ None,
        Instant::now() + Duration::from_millis(50),
        CollectionMode::UntilDeadline,
    )
    .await;
    assert!(second_drain.is_empty());

    let final_output = resolve_aggregated_output(&output_buffer, "FINAL_OUTPUT".to_string()).await;
    assert_eq!(
        final_output
            .matches("256 byte(s) omitted from the middle")
            .count(),
        1
    );
    assert!(!final_output.contains("streaming receiver lagged"));
    assert_eq!(output_buffer.lock().await.omitted_bytes(), 256);
}

#[tokio::test]
async fn many_small_drains_share_one_response_cap_and_report_exact_omission() {
    const CHUNK_BYTES: usize = 4 * 1024;
    const CHUNK_COUNT: usize = 300;
    let total_input = CHUNK_BYTES * CHUNK_COUNT;
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(
        UNIFIED_EXEC_OUTPUT_MAX_BYTES,
    )));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let producer = {
        let output_buffer = Arc::clone(&output_buffer);
        let output_notify = Arc::clone(&output_notify);
        let output_closed = Arc::clone(&output_closed);
        let output_closed_notify = Arc::clone(&output_closed_notify);
        tokio::spawn(async move {
            for _ in 0..CHUNK_COUNT {
                output_buffer
                    .lock()
                    .await
                    .push_chunk(vec![b'x'; CHUNK_BYTES]);
                output_notify.notify_waiters();
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        if output_buffer.lock().await.retained_bytes() == 0 {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("collector should drain each source chunk");
            }
            output_closed.store(true, Ordering::Release);
            output_closed_notify.notify_waiters();
        })
    };

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        /*pause_state*/ None,
        Instant::now() + Duration::from_secs(5),
        CollectionMode::UntilDeadline,
    )
    .await;
    producer.await.expect("producer should finish");

    assert!(collected.len() <= UNIFIED_EXEC_OUTPUT_MAX_BYTES);
    let rendered = String::from_utf8_lossy(&collected);
    assert_eq!(rendered.matches("[output truncated:").count(), 1);
    let reported_omitted = rendered
        .split_once("[output truncated: ")
        .and_then(|(_, suffix)| suffix.split_whitespace().next())
        .and_then(|value| value.parse::<usize>().ok())
        .expect("truncation marker should report omitted bytes");
    let retained_source = collected.iter().filter(|byte| **byte == b'x').count();
    assert_eq!(retained_source + reported_omitted, total_input);
}

#[tokio::test]
async fn output_closure_without_output_returns_immediately() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(32)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(true));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started = Instant::now();

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started + Duration::from_secs(5),
        CollectionMode::UntilDeadline,
    )
    .await;

    assert!(collected.is_empty());
    assert!(started.elapsed() < Duration::from_millis(100));
}

#[tokio::test]
async fn quiet_period_starts_after_evidence_and_returns_live_output() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(32)));
    output_buffer.lock().await.push_chunk(b"READY".to_vec());
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started = Instant::now();

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started + Duration::from_secs(1),
        CollectionMode::AfterQuietPeriod(Duration::from_millis(25)),
    )
    .await;

    assert_eq!(collected, b"READY".to_vec());
    assert!(started.elapsed() >= Duration::from_millis(20));
    assert!(started.elapsed() < Duration::from_millis(250));
}

#[tokio::test]
async fn recovery_evidence_wakes_collection_without_output_bytes() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(128)));
    output_buffer
        .lock()
        .await
        .record_recovery_detail("multiple disjoint recovery gaps".to_string());
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        Instant::now() + Duration::from_secs(1),
        CollectionMode::AfterQuietPeriod(Duration::from_millis(10)),
    )
    .await;

    assert!(
        String::from_utf8(collected)
            .expect("marker is UTF-8")
            .contains("multiple disjoint recovery gaps")
    );
}

#[tokio::test]
async fn continuous_output_stops_at_the_maximum_deadline() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(128)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let producer_buffer = Arc::clone(&output_buffer);
    let producer_notify = Arc::clone(&output_notify);
    let producer = tokio::spawn(async move {
        loop {
            producer_buffer.lock().await.push_chunk(vec![b'x']);
            producer_notify.notify_waiters();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let started = Instant::now();

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started + Duration::from_millis(60),
        CollectionMode::AfterQuietPeriod(Duration::from_millis(20)),
    )
    .await;
    producer.abort();
    let _ = producer.await;

    assert!(!collected.is_empty());
    assert!(started.elapsed() >= Duration::from_millis(40));
    assert!(started.elapsed() < Duration::from_millis(200));
}

#[tokio::test]
async fn process_termination_wakes_collection_without_output() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(32)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let cancel = cancellation_token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();
    });
    let started = Instant::now();

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started + Duration::from_secs(5),
        CollectionMode::UntilDeadline,
    )
    .await;

    assert!(collected.is_empty());
    assert!(started.elapsed() < Duration::from_millis(200));
}

#[test]
fn env_overlay_for_exec_server_keeps_runtime_changes_only() {
    let local_policy_env = HashMap::from([
        ("HOME".to_string(), "/client-home".to_string()),
        ("PATH".to_string(), "/client-path".to_string()),
        ("SHELL_SET".to_string(), "policy".to_string()),
        (
            CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
            "current-profile".to_string(),
        ),
    ]);
    let request_env = HashMap::from([
        ("HOME".to_string(), "/client-home".to_string()),
        ("PATH".to_string(), "/sandbox-path".to_string()),
        ("SHELL_SET".to_string(), "policy".to_string()),
        ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
        (
            CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
            "current-profile".to_string(),
        ),
        (
            "CODEX_SANDBOX_NETWORK_DISABLED".to_string(),
            "1".to_string(),
        ),
    ]);

    assert_eq!(
        env_overlay_for_exec_server(&request_env, &local_policy_env),
        HashMap::from([
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
            (
                CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
                "current-profile".to_string(),
            ),
            (
                "CODEX_SANDBOX_NETWORK_DISABLED".to_string(),
                "1".to_string()
            ),
        ])
    );
}

#[test]
fn exec_env_policy_excludes_runtime_permission_profile() {
    let policy = ShellEnvironmentPolicy {
        r#set: HashMap::from([
            (
                "codex_permission_profile".to_string(),
                "stale-profile".to_string(),
            ),
            ("KEEP".to_string(), "value".to_string()),
        ]),
        ..Default::default()
    };

    assert_eq!(
        exec_env_policy_from_shell_policy(&policy),
        codex_exec_server::ExecEnvPolicy {
            inherit: policy.inherit,
            ignore_default_excludes: policy.ignore_default_excludes,
            exclude: vec![CODEX_PERMISSION_PROFILE_ENV_VAR.to_string()],
            r#set: HashMap::from([("KEEP".to_string(), "value".to_string())]),
            include_only: Vec::new(),
        }
    );
}

#[test]
fn exec_server_params_use_path_uri_and_env_policy_overlay_contract() {
    let cwd: codex_utils_absolute_path::AbsolutePathBuf = std::env::current_dir()
        .expect("current dir")
        .try_into()
        .expect("absolute path");
    let file_system_sandbox_policy =
        codex_protocol::permissions::FileSystemSandboxPolicy::unrestricted();
    let network_sandbox_policy = codex_protocol::permissions::NetworkSandboxPolicy::Restricted;
    let permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    let managed_network = ManagedNetworkSandboxContext {
        loopback_ports: vec![43123],
        allow_local_binding: false,
    };
    let mut request = ExecRequest {
        command: vec!["bash".to_string(), "-lc".to_string(), "true".to_string()],
        cwd: cwd.clone().into(),
        env: HashMap::from([
            ("HOME".to_string(), "/client-home".to_string()),
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
            (
                "HTTP_PROXY".to_string(),
                "http://127.0.0.1:43123".to_string(),
            ),
            ("CODEX_NETWORK_PROXY_ACTIVE".to_string(), "1".to_string()),
            (
                "SSL_CERT_FILE".to_string(),
                "/client/custom-ca.pem".to_string(),
            ),
        ]),
        exec_server_env_config: Some(ExecServerEnvConfig {
            policy: codex_exec_server::ExecEnvPolicy {
                inherit: codex_protocol::config_types::ShellEnvironmentPolicyInherit::Core,
                ignore_default_excludes: false,
                exclude: Vec::new(),
                r#set: HashMap::new(),
                include_only: Vec::new(),
            },
            local_policy_env: HashMap::from([
                ("HOME".to_string(), "/client-home".to_string()),
                ("PATH".to_string(), "/client-path".to_string()),
                (
                    "HTTP_PROXY".to_string(),
                    "http://127.0.0.1:43123".to_string(),
                ),
                ("CODEX_NETWORK_PROXY_ACTIVE".to_string(), "1".to_string()),
                (
                    "SSL_CERT_FILE".to_string(),
                    "/client/custom-ca.pem".to_string(),
                ),
            ]),
        }),
        network: None,
        network_environment_id: None,
        expiration: crate::exec::ExecExpiration::DefaultTimeout,
        capture_policy: crate::exec::ExecCapturePolicy::ShellTool,
        sandbox: codex_sandboxing::SandboxType::None,
        windows_sandbox_policy_cwd: cwd.clone().into(),
        windows_sandbox_workspace_roots: vec![cwd],
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: false,
        permission_profile: permission_profile.clone(),
        file_system_sandbox_policy,
        network_sandbox_policy,
        windows_sandbox_filesystem_overrides: None,
        arg0: None,
        exec_server_sandbox: None,
        exec_server_enforce_managed_network: true,
        exec_server_managed_network: Some(managed_network.clone()),
    };

    let params =
        exec_server_params_for_request(/*process_id*/ 123, &request, /*tty*/ true);

    assert_eq!(params.process_id.as_str(), "123");
    assert_eq!(params.cwd, request.cwd);
    assert!(params.enforce_managed_network);
    assert_eq!(params.managed_network, Some(managed_network));
    assert!(params.env_policy.is_some());
    assert_eq!(
        params.env,
        HashMap::from([
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
            (
                "HTTP_PROXY".to_string(),
                "http://127.0.0.1:43123".to_string(),
            ),
            ("CODEX_NETWORK_PROXY_ACTIVE".to_string(), "1".to_string(),),
        ])
    );
    request.exec_server_sandbox = Some(
        codex_exec_server::FileSystemSandboxContext::from_permission_profile(permission_profile),
    );
    let first =
        exec_server_params_for_request(/*process_id*/ 123, &request, /*tty*/ true);
    let second =
        exec_server_params_for_request(/*process_id*/ 123, &request, /*tty*/ true);
    assert!(first.process_id.as_str().starts_with("123-"));
    assert!(second.process_id.as_str().starts_with("123-"));
    assert_ne!(first.process_id, second.process_id);
}

#[cfg(windows)]
#[test]
fn initial_exec_yield_time_uses_windows_floor() {
    let above_max_yield_time_ms = crate::unified_exec::MAX_YIELD_TIME_MS + 1;

    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 1_000),
        crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS
    );
    assert_eq!(clamp_yield_time(/*yield_time_ms*/ 10_000), 10_000);
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ above_max_yield_time_ms),
        crate::unified_exec::MAX_YIELD_TIME_MS
    );
}

#[cfg(windows)]
#[test]
fn warm_executor_yield_time_does_not_reapply_windows_cold_floor() {
    assert_eq!(
        clamp_yield_time_for_readiness(/*yield_time_ms*/ 250, /*executor_ready*/ true),
        crate::unified_exec::MIN_YIELD_TIME_MS
    );
    assert_eq!(
        clamp_yield_time_for_readiness(/*yield_time_ms*/ 250, /*executor_ready*/ false),
        crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS
    );
}

#[cfg(not(windows))]
#[test]
fn initial_exec_yield_time_has_no_platform_floor() {
    assert_eq!(clamp_yield_time(/*yield_time_ms*/ 1_000), 1_000);
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 1),
        crate::unified_exec::MIN_YIELD_TIME_MS
    );
}

#[tokio::test]
async fn network_denial_fallback_message_names_sandbox_network_proxy() {
    let message = network_denial_message_for_session(/*session*/ None, /*deferred*/ None).await;

    assert_eq!(
        message,
        "Network access was denied by the Codex sandbox network proxy."
    );
}

#[tokio::test]
async fn late_network_denial_grace_observes_cancellation_after_exit() {
    let cancellation = CancellationToken::new();
    let cancellation_for_task = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancellation_for_task.cancel();
    });

    assert!(wait_for_late_network_denial(Some(cancellation)).await);
}

#[tokio::test]
async fn failed_initial_end_for_unstored_process_uses_fallback_output() {
    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        "call-unified-denied".to_string(),
    );
    let request = ExecCommandRequest {
        command: vec![
            "sh".to_string(),
            "-lc".to_string(),
            "echo before".to_string(),
        ],
        command_for_safety: vec![
            "sh".to_string(),
            "-lc".to_string(),
            "echo before".to_string(),
        ],
        attempt_key: crate::tools::command_execution::CommandAttemptKey::new(
            "exec_command",
            "test",
            "test-cwd",
            &["echo before".to_string()],
        ),
        raw_output_artifact: crate::tools::command_output_artifact::RawOutputArtifact::Failed {
            message: "test fixture".to_string(),
            owned_path: None,
            bytes: 0,
        },
        shell_type: crate::shell::ShellType::Sh,
        hook_command: "echo before".to_string(),
        process_id: 123,
        yield_time_ms: 1000,
        max_output_tokens: None,
        #[allow(deprecated)]
        cwd: turn.cwd.clone().into(),
        #[allow(deprecated)]
        sandbox_cwd: turn.cwd.clone().into(),
        turn_environment: turn
            .environments
            .primary()
            .cloned()
            .expect("primary environment"),
        shell_mode: codex_tools::UnifiedExecShellMode::Direct,
        network: None,
        tty: true,
        sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
        additional_permissions: None,
        additional_permissions_preapproved: false,
        justification: None,
        prefix_rule: None,
    };

    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    transcript
        .lock()
        .await
        .push_chunk(b"PARTIAL_TRANSCRIPT".to_vec());

    emit_failed_initial_exec_end_if_unstored(
        /*process_started_alive*/ false,
        &context,
        &request,
        #[allow(deprecated)]
        turn.cwd.clone().into(),
        transcript,
        "PRE_DENIAL_MARKER".to_string(),
        "Network access denied".to_string(),
        Duration::from_millis(7),
    )
    .await;

    let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
        .await
        .expect("timed out waiting for failed command execution item")
        .expect("event channel closed");
    let codex_protocol::protocol::EventMsg::ItemCompleted(completed_event) = event.msg else {
        panic!("expected ItemCompleted event");
    };
    let codex_protocol::items::TurnItem::CommandExecution(item) = completed_event.item else {
        panic!("expected CommandExecution item");
    };
    assert_eq!(item.id, "call-unified-denied");
    assert_eq!(
        item.status,
        codex_protocol::items::CommandExecutionStatus::Failed
    );
    assert_eq!(item.exit_code, Some(-1));
    assert_eq!(item.process_id.as_deref(), Some("123"));
    assert_eq!(
        item.aggregated_output.as_deref(),
        Some("PRE_DENIAL_MARKER\nNetwork access denied")
    );
}

#[test]
fn pruning_prefers_exited_processes_outside_recently_used() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), true),
        (3, now - Duration::from_secs(20), false),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), false),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    assert_eq!(candidate, Some(2));
}

#[test]
fn pruning_falls_back_to_lru_when_no_exited() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), false),
        (3, now - Duration::from_secs(20), false),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), false),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    assert_eq!(candidate, Some(1));
}

#[test]
fn pruning_protects_recent_processes_even_if_exited() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), false),
        (3, now - Duration::from_secs(20), true),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), true),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    // (10) is exited but among the last 8; we should drop the LRU outside that set.
    assert_eq!(candidate, Some(1));
}
