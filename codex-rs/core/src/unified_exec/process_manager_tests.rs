use super::*;
use crate::unified_exec::DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS;
use crate::unified_exec::async_watcher::omitted_output_marker;
use crate::unified_exec::async_watcher::resolve_aggregated_output;
use crate::unified_exec::clamp_yield_time;
use crate::unified_exec::clamp_yield_time_for_readiness;
use codex_network_proxy::ManagedNetworkSandboxContext;
use codex_protocol::config_types::EnvironmentVariablePattern;
use codex_utils_output_truncation::DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS;
use codex_utils_output_truncation::DEFAULT_FAILURE_OUTPUT_TOKENS;
use codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS;
use codex_utils_output_truncation::OutputOutcome;
use codex_utils_output_truncation::resolve_output_limits;
use pretty_assertions::assert_eq;
use tokio::time::Duration;
use tokio::time::Instant;

#[tokio::test]
async fn dropped_process_id_reservation_is_released_before_store_transfer() {
    let manager = UnifiedExecProcessManager::default();
    let reservation = manager.reserve_process_id().await;
    assert_eq!(reservation.process_id(), 1000);
    drop(reservation);

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if manager
                .process_store
                .lock()
                .await
                .reserved_process_ids
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropped reservation should be released");

    let next = manager.reserve_process_id().await;
    assert_eq!(next.process_id(), 1000);
    manager.release_process_id(next.process_id()).await;
}

#[test]
fn coherent_packet_budget_uses_bounded_defaults_and_honors_override() {
    const HARD_LIMIT: usize = 20_000;

    assert_eq!(DEFAULT_SUCCESS_OUTPUT_TOKENS, 4_000);
    assert_eq!(DEFAULT_FAILURE_OUTPUT_TOKENS, 10_000);
    assert_eq!(DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS, 10_000);
    assert_eq!(
        resolve_output_limits(
            None,
            OutputOutcome::Success,
            Some("echo ok"),
            "ok",
            HARD_LIMIT,
        )
        .applied_limit,
        DEFAULT_SUCCESS_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve_output_limits(
            None,
            OutputOutcome::Failure,
            Some("custom-command"),
            "failed",
            HARD_LIMIT,
        )
        .applied_limit,
        DEFAULT_FAILURE_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve_output_limits(
            None,
            OutputOutcome::Success,
            Some("cargo nextest run -p codex-core"),
            "tests passed",
            HARD_LIMIT,
        )
        .applied_limit,
        DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve_output_limits(
            None,
            OutputOutcome::Failure,
            None,
            "Traceback (most recent call last):",
            HARD_LIMIT,
        )
        .applied_limit,
        DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve_output_limits(
            Some(1_234),
            OutputOutcome::Failure,
            Some("cargo test"),
            "test result: FAILED",
            HARD_LIMIT,
        )
        .applied_limit,
        1_234
    );
}

#[test]
fn unified_exec_env_injects_defaults() {
    let env = apply_unified_exec_env(HashMap::new(), &ShellEnvironmentPolicy::default());
    let expected = HashMap::from([
        ("NO_COLOR".to_string(), "1".to_string()),
        ("TERM".to_string(), "dumb".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("LC_CTYPE".to_string(), "C.UTF-8".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ("COLORTERM".to_string(), String::new()),
        ("PAGER".to_string(), UNIFIED_EXEC_PAGER.to_string()),
        ("GIT_PAGER".to_string(), UNIFIED_EXEC_PAGER.to_string()),
        ("GH_PAGER".to_string(), UNIFIED_EXEC_PAGER.to_string()),
        ("CODEX_CI".to_string(), "1".to_string()),
    ]);

    assert_eq!(env, expected);
}

#[test]
fn unified_exec_env_preserves_existing_values() {
    let mut base = HashMap::new();
    base.insert("no_color".to_string(), "0".to_string());
    base.insert("PATH".to_string(), "/usr/bin".to_string());

    let env = apply_unified_exec_env(base, &ShellEnvironmentPolicy::default());

    assert_eq!(env.get("no_color"), Some(&"0".to_string()));
    assert!(!env.contains_key("NO_COLOR"));
    assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
}

#[test]
fn unified_exec_env_respects_shell_environment_policy() {
    let mut policy = ShellEnvironmentPolicy {
        include_only: vec![
            EnvironmentVariablePattern::new_case_insensitive("NO_COLOR"),
            EnvironmentVariablePattern::new_case_insensitive("KEEP"),
        ],
        exclude: vec![EnvironmentVariablePattern::new_case_insensitive("PAGER")],
        ..Default::default()
    };
    policy.r#set.insert("NO_COLOR".to_string(), "0".to_string());
    policy.r#set.insert("KEEP".to_string(), "yes".to_string());

    let env = apply_unified_exec_env(create_env(&policy, None), &policy);

    assert_eq!(
        env,
        HashMap::from([
            ("NO_COLOR".to_string(), "0".to_string()),
            ("KEEP".to_string(), "yes".to_string()),
        ])
    );
}

#[test]
fn unified_exec_env_inherit_none_does_not_add_defaults() {
    let policy = ShellEnvironmentPolicy {
        inherit: ShellEnvironmentPolicyInherit::None,
        ..Default::default()
    };

    // The shell environment supplies PATHEXT for Windows command lookup even
    // with inheritance disabled. Unified exec must add no other defaults.
    let expected = if cfg!(windows) {
        HashMap::from([("PATHEXT".to_string(), ".COM;.EXE;.BAT;.CMD".to_string())])
    } else {
        HashMap::new()
    };
    assert_eq!(
        apply_unified_exec_env(create_env(&policy, None), &policy),
        expected
    );
}

#[tokio::test]
async fn lag_survives_drain_for_finalization_without_duplicate_interim_reports() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(32)));
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

#[tokio::test(start_paused = true)]
async fn initial_output_yields_after_meaningful_output_quiet_period() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(1024)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started_at = Instant::now();

    output_buffer.lock().await.push_chunk(b"ready\n".to_vec());

    let collected = UnifiedExecProcessManager::collect_initial_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started_at + Duration::from_secs(10),
    )
    .await;

    assert_eq!(collected, b"ready\n");
    assert_eq!(Instant::now() - started_at, Duration::from_millis(250));
}

#[tokio::test(start_paused = true)]
async fn background_wait_yields_after_meaningful_output_quiet_period() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(1024)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started_at = Instant::now();

    output_buffer.lock().await.push_chunk(b"ready\n".to_vec());

    let collected = UnifiedExecProcessManager::collect_output_until_progress_or_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started_at + Duration::from_secs(10),
    )
    .await;

    assert_eq!(collected, b"ready\n");
    assert_eq!(Instant::now() - started_at, Duration::from_millis(250));
}

#[tokio::test(start_paused = true)]
async fn orchestration_correctness_output_notification_wakes_owner_wait() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(1024)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started_at = Instant::now();

    let waiter = tokio::spawn({
        let output_buffer = Arc::clone(&output_buffer);
        let output_notify = Arc::clone(&output_notify);
        let output_closed = Arc::clone(&output_closed);
        let output_closed_notify = Arc::clone(&output_closed_notify);
        let cancellation_token = cancellation_token.clone();
        async move {
            UnifiedExecProcessManager::collect_output_until_progress_or_deadline(
                &output_buffer,
                &output_notify,
                &output_closed,
                &output_closed_notify,
                &cancellation_token,
                None,
                started_at + Duration::from_secs(10),
            )
            .await
        }
    });
    tokio::task::yield_now().await;

    output_buffer.lock().await.push_chunk(b"ready\n".to_vec());
    output_notify.notify_waiters();

    assert_eq!(waiter.await.unwrap(), b"ready\n");
    assert_eq!(Instant::now() - started_at, Duration::from_millis(250));
}

#[tokio::test(start_paused = true)]
async fn background_wait_ignores_whitespace_until_meaningful_progress() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(1024)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started_at = Instant::now();

    output_buffer.lock().await.push_chunk(b" \r\n".to_vec());
    let progress_buffer = Arc::clone(&output_buffer);
    let progress_notify = Arc::clone(&output_notify);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        progress_buffer.lock().await.push_chunk(b"ready\n".to_vec());
        progress_notify.notify_waiters();
    });

    let collected = UnifiedExecProcessManager::collect_output_until_progress_or_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started_at + Duration::from_secs(10),
    )
    .await;

    assert_eq!(collected, b" \r\nready\n");
    assert_eq!(Instant::now() - started_at, Duration::from_millis(1_250));
}

#[tokio::test(start_paused = true)]
async fn silent_background_wait_uses_one_owner_deadline() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(1024)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started_at = Instant::now();

    let collected = UnifiedExecProcessManager::collect_output_until_progress_or_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started_at + Duration::from_secs(10),
    )
    .await;

    assert!(collected.is_empty());
    assert_eq!(Instant::now() - started_at, Duration::from_secs(10));
}

#[tokio::test(start_paused = true)]
async fn initial_output_quiet_yield_is_clamped_to_hard_deadline() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(1024)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started_at = Instant::now();

    output_buffer.lock().await.push_chunk(b"ready\n".to_vec());

    let collected = UnifiedExecProcessManager::collect_initial_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started_at + Duration::from_millis(100),
    )
    .await;

    assert_eq!(collected, b"ready\n");
    assert_eq!(Instant::now() - started_at, Duration::from_millis(100));
}

#[tokio::test(start_paused = true)]
async fn nonempty_write_collection_honors_the_requested_deadline() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(1024)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started_at = Instant::now();

    output_buffer
        .lock()
        .await
        .push_chunk(b"progress\n".to_vec());

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started_at + Duration::from_millis(100),
    )
    .await;

    assert_eq!(collected, b"progress\n");
    assert_eq!(Instant::now() - started_at, Duration::from_millis(100));
}

#[tokio::test(start_paused = true)]
async fn initial_output_post_exit_uses_quiet_deadline_instead_of_full_yield() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(1024)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started_at = Instant::now();

    output_buffer.lock().await.push_chunk(b"done\n".to_vec());
    cancellation_token.cancel();

    let collected = UnifiedExecProcessManager::collect_initial_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started_at + Duration::from_secs(10),
    )
    .await;

    assert_eq!(collected, b"done\n");
    assert_eq!(Instant::now() - started_at, Duration::from_millis(250));
}

#[tokio::test(start_paused = true)]
async fn background_poll_post_exit_does_not_inherit_the_five_second_poll_deadline() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(1024)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started_at = Instant::now();

    cancellation_token.cancel();

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started_at + Duration::from_secs(5),
    )
    .await;

    assert!(collected.is_empty());
    assert_eq!(Instant::now() - started_at, Duration::from_millis(250));
}

#[tokio::test(start_paused = true)]
async fn initial_output_post_exit_quiet_deadline_resets_after_tail_output() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(1024)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started_at = Instant::now();

    cancellation_token.cancel();
    let tail_buffer = Arc::clone(&output_buffer);
    let tail_notify = Arc::clone(&output_notify);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        tail_buffer.lock().await.push_chunk(b"tail\n".to_vec());
        tail_notify.notify_waiters();
    });

    let collected = UnifiedExecProcessManager::collect_initial_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started_at + Duration::from_secs(10),
    )
    .await;

    assert_eq!(collected, b"tail\n");
    assert_eq!(Instant::now() - started_at, Duration::from_millis(450));
}

#[tokio::test(start_paused = true)]
async fn initial_output_whitespace_returns_a_live_handle_promptly() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(1024)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let started_at = Instant::now();

    output_buffer.lock().await.push_chunk(b" \r\n\t".to_vec());

    let collected = UnifiedExecProcessManager::collect_initial_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        None,
        started_at + Duration::from_secs(2),
    )
    .await;

    assert_eq!(collected, b" \r\n\t");
    assert_eq!(Instant::now() - started_at, Duration::from_millis(250));
}

#[tokio::test]
async fn capacity_omission_is_reported_once_per_drain_and_preserved_for_finalization() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::new(8)));
    output_buffer
        .lock()
        .await
        .push_chunk(b"0123456789abcdef".to_vec());
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
    )
    .await;
    let collected = String::from_utf8(collected).expect("collected output is UTF-8");
    assert_eq!(
        collected,
        format!(
            "0123{}cdef",
            String::from_utf8(omitted_output_marker(8)).expect("marker is UTF-8")
        )
    );
    assert_eq!(
        collected
            .matches("8 byte(s) omitted from the middle")
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
    )
    .await;
    assert!(second_drain.is_empty());

    let final_output = resolve_aggregated_output(&output_buffer, "FINAL_OUTPUT".to_string()).await;
    assert_eq!(
        final_output
            .matches("8 byte(s) omitted from the middle")
            .count(),
        1
    );
    assert!(!final_output.contains("streaming receiver lagged"));
    assert_eq!(output_buffer.lock().await.omitted_bytes(), 8);
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

fn exec_server_request_for_env_test() -> ExecRequest {
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
    ExecRequest {
        command: vec!["bash".to_string(), "-lc".to_string(), "true".to_string()],
        codex_home: cwd.clone(),
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
        windows_sandbox_additional_read_roots: Vec::new(),
        arg0: None,
        exec_server_sandbox: None,
        exec_server_enforce_managed_network: true,
        exec_server_managed_network: Some(managed_network),
    }
}

#[tokio::test]
async fn exec_server_params_use_path_uri_and_env_policy_overlay_contract() {
    let mut request = exec_server_request_for_env_test();
    let managed_network = request.exec_server_managed_network.clone().unwrap();
    let permission_profile = request.permission_profile.clone();

    let params =
        exec_server_params_for_request(/*process_id*/ 123, &request, /*tty*/ true)
            .await
            .unwrap();

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
        exec_server_params_for_request(/*process_id*/ 123, &request, /*tty*/ true)
            .await
            .unwrap();
    let second =
        exec_server_params_for_request(/*process_id*/ 123, &request, /*tty*/ true)
            .await
            .unwrap();
    assert!(first.process_id.as_str().starts_with("123-"));
    assert!(second.process_id.as_str().starts_with("123-"));
    assert_ne!(first.process_id, second.process_id);
}

#[test]
fn remote_ca_environment_waits_for_worker_and_preserves_peer_hash_contract() {
    const CHILD: &str = "KDA_REMOTE_CA_ENV_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unified_exec::process_manager::tests::remote_ca_environment_waits_for_worker_and_preserves_peer_hash_contract",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("CODEX_HOME", home.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated CA behavior failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    use futures::SinkExt;
    use futures::StreamExt;
    use sha2::Digest;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let home = std::path::PathBuf::from(std::env::var_os("CODEX_HOME").unwrap());
        let proxy_dir = home.join("proxy");
        std::fs::create_dir_all(&proxy_dir).unwrap();
        let certificate_bytes = b"independent generated trust bundle bytes\n";
        let hash = format!("{:x}", sha2::Sha256::digest(certificate_bytes));
        let bundle = proxy_dir.join(format!("ca-bundle-{hash}.pem"));
        std::fs::write(&bundle, certificate_bytes).unwrap();
        let bundle_text = bundle.to_string_lossy().into_owned();

        // Only the external peer is substituted: normal environment setup, request
        // preparation, remote backend and JSON-RPC serialization remain in use.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (captured_tx, mut captured_rx) = tokio::sync::mpsc::unbounded_channel();
        let peer = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut started_processes = std::collections::HashSet::new();
            while let Some(frame) = socket.next().await {
                let frame = frame.unwrap();
                let tokio_tungstenite::tungstenite::Message::Text(text) = frame else { continue; };
                let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                let response = match request["method"].as_str().unwrap() {
                    "initialize" => serde_json::json!({"id": request["id"], "result": {"sessionId": "ca-env-peer"}}),
                    "initialized" => continue,
                    "environment/info" => serde_json::json!({"id": request["id"], "result": {
                        "operatingSystem": "windows",
                        "shell": {"name": "cmd", "path": "cmd.exe"},
                        "cwd": "file:///C:/workspace"
                    }}),
                    "process/start" => {
                        started_processes.insert(request["params"]["processId"].as_str().unwrap().to_string());
                        captured_tx.send(request["params"].clone()).unwrap();
                        serde_json::json!({"id": request["id"], "error": {"code": -32000, "message": "peer captured launch"}})
                    }
                    "process/terminate" => {
                        assert!(started_processes.contains(request["params"]["processId"].as_str().unwrap()),
                            "cleanup must correspond to a start the peer actually received");
                        serde_json::json!({"id": request["id"], "result": {"running": false}})
                    }
                    method => panic!("unexpected peer request: {method}"),
                };
                socket.send(tokio_tungstenite::tungstenite::Message::Text(response.to_string().into())).await.unwrap();
            }
        }));
        let environment = codex_exec_server::Environment::create_for_tests(Some(url)).unwrap();
        environment.wait_until_ready().await.unwrap();
        let manager = UnifiedExecProcessManager::default();
        let pending_spawns = PendingSpawnRegistration::default();
        let mut request = exec_server_request_for_env_test();
        request.env.insert("SSL_CERT_FILE".to_string(), bundle_text.clone());
        request.exec_server_env_config.as_mut().unwrap().local_policy_env
            .insert("SSL_CERT_FILE".to_string(), bundle_text.clone());

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).is_ok()
        });
        started_rx.await.unwrap();
        let mut canceled_launch = Box::pin(manager.open_session_with_prepared_exec_env(
            900, &request, false, Box::new(crate::unified_exec::NoopSpawnLifecycle),
            None, &environment, &pending_spawns,
        ));
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut canceled_launch).await.is_err(),
            "native CA read/hash must wait for the worker while the caller timer advances");
        assert!(captured_rx.try_recv().is_err(), "no launch may reach the peer before CA preparation");
        assert!(!blocker.is_finished());
        drop(canceled_launch);
        release_tx.send(()).unwrap();
        assert!(blocker.await.unwrap());
        tokio::task::spawn_blocking(|| {}).await.unwrap();
        assert!(captured_rx.try_recv().is_err(), "abandoned preparation must not launch after its worker finishes");

        // Rejected starts retain their ID until asynchronous peer cleanup settles.
        // These independent CA cases must use distinct process identities.
        for (process_id, valid) in [(901, true), (902, false)] {
            if !valid {
                std::fs::write(&bundle, b"tampered bytes under the same generated filename").unwrap();
            }
            let result = manager.open_session_with_prepared_exec_env(
                process_id, &request, false, Box::new(crate::unified_exec::NoopSpawnLifecycle),
                None, &environment, &pending_spawns,
            ).await;
            let error = result.err().expect("external peer declines after capturing exact request");
            assert!(error.to_string().contains("peer captured launch"), "unexpected launch error: {error:?}");
            let params: codex_exec_server::ExecParams = serde_json::from_value(captured_rx.recv().await.unwrap()).unwrap();
            assert_eq!(params.process_id.as_str(), process_id.to_string());
            assert_eq!(params.env.get("SSL_CERT_FILE"), valid.then_some(&bundle_text));
            assert_eq!(params.env.get("HTTP_PROXY"), request.env.get("HTTP_PROXY"));
            assert_eq!(params.env_policy, Some(request.exec_server_env_config.as_ref().unwrap().policy.clone()));
            assert_eq!(params.argv, request.command);
            assert_eq!(params.cwd, request.cwd);
            assert_eq!(params.managed_network, request.exec_server_managed_network);
            assert!(manager.process_store.lock().await.processes.is_empty(), "rejected peer start must not publish a local process owner");
        }
        assert_eq!(std::fs::read(&bundle).unwrap(), b"tampered bytes under the same generated filename");
        drop(environment);
        peer.abort();
        let _ = peer.await;
    });
}

#[test]
fn initial_exec_yield_time_uses_platform_floor() {
    let above_max_yield_time_ms = crate::unified_exec::MAX_YIELD_TIME_MS + 1;
    let expected_initial_yield_time_ms = if cfg!(windows) {
        crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS
    } else {
        1_000
    };

    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 1_000),
        expected_initial_yield_time_ms
    );
    assert_eq!(clamp_yield_time(/*yield_time_ms*/ 10_000), 10_000);
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ above_max_yield_time_ms),
        crate::unified_exec::MAX_YIELD_TIME_MS
    );
}

#[cfg(windows)]
#[tokio::test]
async fn remote_registration_failure_preserves_original_error_when_cleanup_also_fails() {
    use futures::SinkExt;
    use futures::StreamExt;

    let (session, mut turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let turn_mut = Arc::get_mut(&mut turn).expect("unique turn fixture");
    turn_mut.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    turn_mut
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::Never)
        .unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
    // Substitute only the external peer: the normal environment, remote backend,
    // manager registration, ledger rejection and pending cleanup all execute.
    let peer = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut termination_count = 0;
        while let Some(frame) = socket.next().await {
            let tokio_tungstenite::tungstenite::Message::Text(text) = frame.unwrap() else {
                continue;
            };
            let request: serde_json::Value = serde_json::from_str(&text).unwrap();
            let response = match request["method"].as_str().unwrap() {
                "initialize" => {
                    serde_json::json!({"id": request["id"], "result": {"sessionId": "ledger-failure-peer"}})
                }
                "initialized" => continue,
                "environment/info" => serde_json::json!({"id": request["id"], "result": {
                    "operatingSystem": "windows", "shell": {"name": "powershell", "path": "powershell.exe"},
                    "cwd": "file:///C:/workspace"
                }}),
                "process/start" => {
                    requests_tx.send(request.clone()).unwrap();
                    serde_json::json!({"id": request["id"], "result": {"processId": request["params"]["processId"]}})
                }
                "process/terminate" => {
                    termination_count += 1;
                    let _ = requests_tx.send(request.clone());
                    let message = if termination_count == 1 {
                        "ledger cleanup refused"
                    } else {
                        "pending cleanup refused"
                    };
                    serde_json::json!({"id": request["id"], "error": {"code": -32000, "message": message}})
                }
                method => panic!("unexpected peer request: {method}"),
            };
            if socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    response.to_string().into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    }));
    let remote = Arc::new(codex_exec_server::Environment::create_for_tests(Some(url)).unwrap());
    remote.wait_until_ready().await.unwrap();
    let manager = &session.services.unified_exec_manager;
    let reservation = manager.reserve_process_id().await;
    let process_id = reservation.process_id();
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        "registration-failure".to_string(),
    );
    let command = vec![
        "powershell.exe".to_string(),
        "-Command".to_string(),
        "echo registered".to_string(),
    ];
    let attempt_key = crate::tools::command_execution::CommandAttemptKey::new(
        "exec_command",
        "remote",
        turn.cwd().to_string_lossy(),
        &command,
    );
    let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
        fixture.path(),
        "registration-failure",
        b"",
    )
    .await;
    let existing_key = crate::tools::command_execution::CommandAttemptKey::new(
        "exec_command",
        "remote",
        turn.cwd().to_string_lossy(),
        &["existing command".to_string()],
    );
    session
        .services
        .command_execution
        .track_running_process(process_id, existing_key.clone(), artifact.clone())
        .await
        .unwrap();
    let existing_identity = session
        .services
        .command_execution
        .process_execution_identity(process_id)
        .await
        .unwrap();
    let request = ExecCommandRequest {
        validation: None,
        command: command.clone(),
        command_for_safety: command.clone(),
        attempt_key,
        raw_output_artifact: artifact,
        shell_type: crate::shell::ShellType::PowerShell,
        shell_wrapper_is_owned: false,
        hook_command: "echo registered".to_string(),
        process_id,
        yield_time_ms: 30_000,
        max_output_tokens: None,
        cwd: turn.cwd().clone().into(),
        normalization_cwd: None,
        sandbox_cwd: turn.cwd().clone().into(),
        turn_environment: crate::session::turn_context::TurnEnvironment::new(
            "remote".to_string(),
            remote,
            turn.cwd().clone().into(),
            None,
        ),
        network: None,
        tty: false,
        sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
        additional_permissions: None,
        additional_permissions_uri: None,
        additional_permissions_preapproved: false,
        justification: None,
        prefix_rule: None,
        validation_launch: None,
        known_delta: None,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        manager.exec_command(request, reservation, &context, &CancellationToken::new()),
    )
    .await
    .expect("normal manager failure settles");
    let Err(UnifiedExecError::ProcessFailed { message }) = result else {
        panic!("expected registration failure: {result:?}");
    };
    assert!(
        message.contains(&format!(
            "process id {process_id} already has live command bookkeeping"
        )),
        "{message}"
    );
    assert!(
        message.contains("additionally failed to terminate the untracked process"),
        "{message}"
    );
    assert!(message.contains("ledger cleanup refused"), "{message}");
    assert!(
        message.contains("unified exec startup cleanup failed"),
        "{message}"
    );
    assert!(message.contains("pending cleanup refused"), "{message}");
    let start = requests_rx
        .try_recv()
        .expect("normal backend sent process/start");
    assert_eq!(start["method"], "process/start");
    assert_eq!(
        start["params"]["argv"],
        serde_json::json!([
            "powershell.exe",
            "-Command",
            "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\necho registered"
        ])
    );
    for _ in 0..2 {
        let terminate = requests_rx
            .try_recv()
            .expect("both actual cleanup boundaries reached peer");
        assert_eq!(terminate["method"], "process/terminate");
        assert_eq!(
            terminate["params"]["processId"],
            start["params"]["processId"]
        );
    }
    assert_eq!(
        session
            .services
            .command_execution
            .process_execution_identity(process_id)
            .await,
        Some(existing_identity)
    );
    assert_eq!(
        session
            .services
            .command_execution
            .running_process(process_id)
            .await
            .unwrap()
            .key,
        existing_key
    );
    assert!(
        !manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&process_id)
    );
    peer.abort();
    let _ = peer.await;
}

#[cfg(windows)]
#[tokio::test]
async fn remote_startup_cleanup_failure_retains_native_child_until_session_shutdown_retry() {
    use futures::SinkExt;
    use futures::StreamExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    let (session, mut turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let turn_mut = Arc::get_mut(&mut turn).expect("unique turn fixture");
    turn_mut.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    turn_mut
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::Never)
        .unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
    let allow_termination = Arc::new(AtomicBool::new(false));
    let peer_allow_termination = Arc::clone(&allow_termination);
    // Substitute only the external peer: the normal environment, remote backend,
    // manager registration, ledger rejection and pending cleanup all execute.
    let peer = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut termination_count = 0;
        let mut native_child: Option<tokio::process::Child> = None;
        while let Some(frame) = socket.next().await {
            let tokio_tungstenite::tungstenite::Message::Text(text) = frame.unwrap() else {
                continue;
            };
            let request: serde_json::Value = serde_json::from_str(&text).unwrap();
            let response = match request["method"].as_str().unwrap() {
                "initialize" => {
                    serde_json::json!({"id": request["id"], "result": {"sessionId": "ledger-failure-peer"}})
                }
                "initialized" => continue,
                "environment/info" => serde_json::json!({"id": request["id"], "result": {
                    "operatingSystem": "windows", "shell": {"name": "powershell", "path": "powershell.exe"},
                    "cwd": "file:///C:/workspace"
                }}),
                "process/start" => {
                    let argv = request["params"]["argv"].as_array().unwrap();
                    let child = tokio::process::Command::new(argv[0].as_str().unwrap())
                        .args(argv[1..].iter().map(|arg| arg.as_str().unwrap()))
                        .kill_on_drop(true)
                        .spawn()
                        .expect("external peer starts actual child");
                    let mut observed = request.clone();
                    observed["nativePid"] = serde_json::json!(child.id().unwrap());
                    requests_tx.send(observed).unwrap();
                    native_child = Some(child);
                    serde_json::json!({"id": request["id"], "result": {"processId": request["params"]["processId"]}})
                }
                "process/terminate" => {
                    termination_count += 1;
                    let _ = requests_tx.send(request.clone());
                    if peer_allow_termination.load(Ordering::Acquire) {
                        let child = native_child.as_mut().expect("same live native child");
                        child
                            .kill()
                            .await
                            .expect("shutdown retry kills native child");
                        child
                            .wait()
                            .await
                            .expect("external peer reaps native child");
                        serde_json::json!({"id": request["id"], "result": {"running": false}})
                    } else {
                        let message = if termination_count == 1 {
                            "ledger cleanup refused"
                        } else {
                            "pending cleanup refused"
                        };
                        serde_json::json!({"id": request["id"], "error": {"code": -32000, "message": message}})
                    }
                }
                method => panic!("unexpected peer request: {method}"),
            };
            if socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    response.to_string().into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    }));
    let remote = Arc::new(codex_exec_server::Environment::create_for_tests(Some(url)).unwrap());
    remote.wait_until_ready().await.unwrap();
    let manager = &session.services.unified_exec_manager;
    let reservation = manager.reserve_process_id().await;
    let process_id = reservation.process_id();
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        "registration-failure".to_string(),
    );
    let command = vec![
        "powershell.exe".to_string(),
        "-Command".to_string(),
        "Start-Sleep -Seconds 60".to_string(),
    ];
    let attempt_key = crate::tools::command_execution::CommandAttemptKey::new(
        "exec_command",
        "remote",
        turn.cwd().to_string_lossy(),
        &command,
    );
    let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
        fixture.path(),
        "registration-failure",
        b"",
    )
    .await;
    let existing_key = crate::tools::command_execution::CommandAttemptKey::new(
        "exec_command",
        "remote",
        turn.cwd().to_string_lossy(),
        &["existing command".to_string()],
    );
    session
        .services
        .command_execution
        .track_running_process(process_id, existing_key.clone(), artifact.clone())
        .await
        .unwrap();
    let existing_identity = session
        .services
        .command_execution
        .process_execution_identity(process_id)
        .await
        .unwrap();
    let request = ExecCommandRequest {
        validation: None,
        command: command.clone(),
        command_for_safety: command.clone(),
        attempt_key,
        raw_output_artifact: artifact,
        shell_type: crate::shell::ShellType::PowerShell,
        shell_wrapper_is_owned: false,
        hook_command: "Start-Sleep -Seconds 60".to_string(),
        process_id,
        yield_time_ms: 30_000,
        max_output_tokens: None,
        cwd: turn.cwd().clone().into(),
        normalization_cwd: None,
        sandbox_cwd: turn.cwd().clone().into(),
        turn_environment: crate::session::turn_context::TurnEnvironment::new(
            "remote".to_string(),
            remote,
            turn.cwd().clone().into(),
            None,
        ),
        network: None,
        tty: false,
        sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
        additional_permissions: None,
        additional_permissions_uri: None,
        additional_permissions_preapproved: false,
        justification: None,
        prefix_rule: None,
        validation_launch: None,
        known_delta: None,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        manager.exec_command(request, reservation, &context, &CancellationToken::new()),
    )
    .await
    .expect("normal manager failure settles");
    let Err(UnifiedExecError::ProcessFailed { message }) = result else {
        panic!("expected registration failure: {result:?}");
    };
    assert!(
        message.contains(&format!(
            "process id {process_id} already has live command bookkeeping"
        )),
        "{message}"
    );
    assert!(
        message.contains("additionally failed to terminate the untracked process"),
        "{message}"
    );
    assert!(message.contains("ledger cleanup refused"), "{message}");
    assert!(
        message.contains("unified exec startup cleanup failed"),
        "{message}"
    );
    assert!(message.contains("pending cleanup refused"), "{message}");
    let start = requests_rx
        .try_recv()
        .expect("normal backend sent process/start");
    let raw = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE,
            0,
            start["nativePid"].as_u64().unwrap() as u32,
        )
    };
    assert!(!raw.is_null(), "independent native child handle");
    let child = unsafe { OwnedHandle::from_raw_handle(raw) };
    assert_eq!(
        unsafe { WaitForSingleObject(child.as_raw_handle(), 0) },
        WAIT_TIMEOUT
    );
    assert_eq!(start["method"], "process/start");
    assert_eq!(start["params"]["argv"], serde_json::json!([
        "powershell.exe", "-Command",
        "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\nStart-Sleep -Seconds 60",
    ]));
    for _ in 0..2 {
        let terminate = requests_rx
            .try_recv()
            .expect("both actual cleanup boundaries reached peer");
        assert_eq!(terminate["method"], "process/terminate");
        assert_eq!(
            terminate["params"]["processId"],
            start["params"]["processId"]
        );
    }
    assert_eq!(
        session
            .services
            .command_execution
            .process_execution_identity(process_id)
            .await,
        Some(existing_identity.clone())
    );
    assert_eq!(
        session
            .services
            .command_execution
            .running_process(process_id)
            .await
            .unwrap()
            .key,
        existing_key
    );
    assert!(
        !manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&process_id)
    );
    session.terminal_tasks.close();
    tokio::time::timeout(Duration::from_secs(5), session.terminal_tasks.wait())
        .await
        .expect("failed Drop cleanup must settle and retain ownership");
    assert_eq!(
        manager
            .pending_cleanup_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1,
        "failed provisional cleanup must remain discoverable for shutdown"
    );
    assert_eq!(
        unsafe { WaitForSingleObject(child.as_raw_handle(), 0) },
        WAIT_TIMEOUT,
        "a rejection must not be reported as native process exit"
    );
    let retry_owner = requests_rx
        .recv()
        .await
        .expect("Drop cleanup reached external peer");
    assert_eq!(retry_owner["method"], "process/terminate");
    assert_eq!(
        retry_owner["params"]["processId"],
        start["params"]["processId"]
    );
    allow_termination.store(true, Ordering::Release);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), session.shutdown_runtime_for_test())
            .await
            .expect("normal shutdown retries retained cleanup")
    );
    assert_eq!(
        unsafe { WaitForSingleObject(child.as_raw_handle(), 0) },
        WAIT_OBJECT_0
    );
    assert!(
        manager
            .pending_cleanup_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
    assert_eq!(
        session
            .services
            .command_execution
            .process_execution_identity(process_id)
            .await,
        Some(existing_identity),
        "cleanup must preserve unrelated bookkeeping under the same process id"
    );
    peer.abort();
    let _ = peer.await;
}

#[test]
fn warm_executor_yield_time_does_not_reapply_windows_cold_floor() {
    assert_eq!(
        clamp_yield_time_for_readiness(/*yield_time_ms*/ 250, /*executor_ready*/ true),
        crate::unified_exec::MIN_YIELD_TIME_MS
    );
    let expected_cold_yield_time_ms = if cfg!(windows) {
        crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS
    } else {
        crate::unified_exec::MIN_YIELD_TIME_MS
    };
    assert_eq!(
        clamp_yield_time_for_readiness(/*yield_time_ms*/ 250, /*executor_ready*/ false),
        expected_cold_yield_time_ms
    );
}

#[test]
fn executor_readiness_is_scoped_to_environment() {
    let manager = UnifiedExecProcessManager::new_with_deferred_executor(
        DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS,
        /*deferred_executor_enabled*/ false,
    );

    assert!(!manager.mark_executor_ready("environment-a"));
    assert!(manager.mark_executor_ready("environment-a"));
    assert!(!manager.mark_executor_ready("environment-b"));
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
        validation: None,
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
            id: None,
            message: "test fixture".to_string(),
            owned_path: None,
            bytes: 0,
        },
        shell_type: crate::shell::ShellType::Sh,
        shell_wrapper_is_owned: true,
        hook_command: "echo before".to_string(),
        process_id: 123,
        yield_time_ms: 1000,
        max_output_tokens: None,
        cwd: turn.cwd().clone().into(),

        normalization_cwd: None,
        sandbox_cwd: turn.cwd().clone().into(),
        turn_environment: turn
            .environments
            .primary()
            .cloned()
            .expect("primary environment"),
        network: None,
        tty: true,
        sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
        additional_permissions: None,
        additional_permissions_uri: None,
        additional_permissions_preapproved: false,
        justification: None,
        prefix_rule: None,
        validation_launch: None,
        known_delta: None,
    };

    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    transcript
        .lock()
        .await
        .push_chunk(b"PARTIAL_TRANSCRIPT".to_vec());

    emit_failed_initial_exec_end_if_unstored(
        /*process_started_alive*/ false,
        None,
        &context,
        &request,
        turn.cwd().clone().into(),
        transcript,
        "PRE_DENIAL_MARKER".to_string(),
        "Network access denied".to_string(),
        Duration::from_millis(7),
    )
    .await
    .expect("terminal failure event has no pending persistence failure");

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
fn pruning_refuses_to_evict_when_no_process_has_exited() {
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

    assert_eq!(candidate, None);
}

#[test]
fn pruning_selects_the_oldest_exited_process_without_evicting_live_processes() {
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

    assert_eq!(candidate, Some(3));
}

#[tokio::test]
async fn exited_process_rejects_success_when_terminal_watcher_disappears() {
    let python = which::which("python")
        .or_else(|_| which::which("python3"))
        .expect("Python is required by the unified-exec receipt test");
    let workspace = tempfile::tempdir().expect("temporary process workspace");
    let spawned = codex_utils_pty::spawn_pipe_process_no_stdin(
        &python.to_string_lossy(),
        &["-c".to_string(), "print('completed')".to_string()],
        workspace.path(),
        &HashMap::new(),
        &None,
    )
    .await
    .expect("actual process starts");
    let process = UnifiedExecProcess::from_spawned(
        spawned,
        codex_sandboxing::SandboxType::None,
        Box::new(crate::unified_exec::NoopSpawnLifecycle),
        None,
        &PendingSpawnRegistration::default(),
    )
    .await
    .expect("actual process registers");
    tokio::time::timeout(
        Duration::from_secs(5),
        process.cancellation_token().cancelled(),
    )
    .await
    .expect("actual process exits");
    let receipt = process.register_terminal_completion();
    drop(receipt);
    let response = ExecCommandToolOutput {
        validation: None,
        event_call_id: "completed-without-watcher".to_string(),
        chunk_id: "chunk".to_string(),
        wall_time: Duration::from_millis(1),
        raw_output: b"completed".to_vec(),
        truncation_policy: codex_utils_output_truncation::TruncationPolicy::Tokens(100),
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        process_exited: true,
        original_token_count: None,
        hook_command: None,
        raw_output_artifact: None,
        raw_output_reduction_notice: None,
        repair_notice: None,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        finish_exited_process_result(Some(&process), Ok(response), Duration::from_millis(1)),
    )
    .await
    .expect("closed receipt must not hang");
    assert!(
        matches!(result, Err(UnifiedExecError::ToolHistoryPersistence { message, exit_code: 0, .. })
        if message == "unified exec exit watcher closed before terminal finalization")
    );
}

#[cfg(windows)]
#[test]
fn pending_remote_exec_dropped_outside_runtime_terminates_native_child() {
    check_pending_remote_exec_drop(false);
}

#[cfg(windows)]
#[test]
fn pending_remote_exec_drop_is_owned_through_normal_session_shutdown() {
    check_pending_remote_exec_drop(true);
}

#[cfg(windows)]
fn check_pending_remote_exec_drop(entered_shutdown: bool) {
    std::thread::Builder::new()
        .name("pending-remote-exec-drop".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            use std::os::windows::io::AsRawHandle;
            use std::os::windows::io::FromRawHandle;
            use std::os::windows::io::OwnedHandle;
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
            use windows_sys::Win32::System::Threading::OpenProcess;
            use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
            use windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
            use windows_sys::Win32::System::Threading::TerminateProcess;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;

            // Own an independent native handle, including failure cleanup, so
            // a missing remote termination cannot leak the test child.
            struct ChildGuard(OwnedHandle);
            impl Drop for ChildGuard {
                fn drop(&mut self) {
                    unsafe {
                        if WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_TIMEOUT {
                            TerminateProcess(self.0.as_raw_handle(), 1);
                            WaitForSingleObject(self.0.as_raw_handle(), 5_000);
                        }
                    }
                }
            }
            let mut builder = if entered_shutdown {
                tokio::runtime::Builder::new_current_thread()
            } else {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.worker_threads(2);
                builder
            };
            let runtime = builder.thread_stack_size(16 * 1024 * 1024)
                .enable_all().build().expect("live transport runtime");
            let fixture = tempfile::tempdir().expect("child fixture");
            let marker = fixture.path().join("native-pid.txt");
            let (session, mut turn, events) = runtime.block_on(
                crate::session::tests::make_session_and_context_with_rx(),
            );
            let turn_mut = Arc::get_mut(&mut turn).expect("unique turn fixture");
            turn_mut.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
            turn_mut.approval_policy.set(codex_protocol::protocol::AskForApproval::Never)
                .expect("unrestricted fixture approval policy");
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve server address");
            let address = probe.local_addr().expect("server address");
            drop(probe);
            let url = format!("ws://{address}");
            let server_url = url.clone();
            let server = tokio_util::task::AbortOnDropHandle::new(runtime.spawn(async move {
                // Unrestricted commands do not invoke sandbox helper modes.
                let paths = codex_exec_server::ExecServerRuntimePaths::new(
                    std::env::current_exe().expect("test executable path"),
                ).expect("absolute executable path");
                codex_exec_server::run_main(&server_url, paths).await
                    .expect("real exec server");
            }));
            let remote = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if tokio::net::TcpStream::connect(address).await.is_ok() { break; }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    let environment = Arc::new(codex_exec_server::Environment::create_for_tests(Some(url))
                        .expect("real remote environment"));
                    environment.wait_until_ready().await.expect("remote handshake");
                    environment
                }).await.expect("exec server readiness deadline")
            });
            let manager = &session.services.unified_exec_manager;
            let reservation = runtime.block_on(manager.reserve_process_id());
            let process_id = reservation.process_id();
            let context = UnifiedExecContext::new(Arc::clone(&session), Arc::clone(&turn), "outside-runtime".to_string());
            let script = format!(
                "[System.IO.File]::WriteAllText('{}', [string]$PID); Start-Sleep -Seconds 60",
                marker.to_string_lossy().replace('\'', "''"),
            );
            let command = vec!["powershell.exe".to_string(), "-NoLogo".to_string(), "-NoProfile".to_string(), "-NonInteractive".to_string(), "-Command".to_string(), script.clone()];
            let attempt_key = crate::tools::command_execution::CommandAttemptKey::new(
                "exec_command", "remote", turn.cwd().to_string_lossy(), &command,
            );
            let artifact = runtime.block_on(crate::tools::command_output_artifact::create_raw_output_artifact(
                fixture.path(), "outside-runtime", b"",
            ));
            let request = ExecCommandRequest {
                validation: None,                command: command.clone(), command_for_safety: command,
                attempt_key, raw_output_artifact: artifact,
                shell_type: crate::shell::ShellType::PowerShell, shell_wrapper_is_owned: false,
                hook_command: script, process_id, yield_time_ms: 30_000, max_output_tokens: None,
                cwd: turn.cwd().clone().into(), normalization_cwd: None,
                sandbox_cwd: turn.cwd().clone().into(),
                turn_environment: crate::session::turn_context::TurnEnvironment::new(
                    "remote".to_string(), remote, turn.cwd().clone().into(), None,
                ),
                network: None, tty: false,
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None, additional_permissions_uri: None,
                additional_permissions_preapproved: false, justification: None, prefix_rule: None,
                validation_launch: None, known_delta: None,
            };
            let cancellation = CancellationToken::new();
            // Scheduling control only: the normal execution must attach the
            // remote process, then wait here before store insertion/commit.
            let store_guard = runtime.block_on(manager.process_store.lock());
            let mut pending = Box::pin(manager.exec_command(request, reservation, &context, &cancellation));
            let pid = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(15), async {
                    let observation = async {
                        loop {
                            let event = events.recv().await.expect("normal command event");
                            if let codex_protocol::protocol::EventMsg::ExecCommandBegin(begin) = event.msg {
                                assert_eq!(begin.call_id, "outside-runtime");
                                break;
                            }
                        }
                        // ExecCommandBegin is emitted only after attach_process;
                        // the PID marker independently identifies the OS child.
                        loop {
                            if let Ok(value) = tokio::fs::read_to_string(&marker).await
                                && let Ok(pid) = value.parse::<u32>() { break pid; }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    };
                    tokio::select! {
                        result = &mut pending => panic!("exec completed before pending-registration barrier: {result:?}"),
                        pid = observation => pid,
                    }
                }).await.expect("registered live remote command")
            });
            let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
            assert!(!raw.is_null(), "open native child: {}", std::io::Error::last_os_error());
            let child = ChildGuard(unsafe { OwnedHandle::from_raw_handle(raw) });
            assert_eq!(unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) }, WAIT_TIMEOUT);
            if entered_shutdown {
                runtime.block_on(async {
                    drop(pending);
                    let cleanup = manager.pending_cleanup_owners.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)[0].clone();
                    let process = Arc::clone(&cleanup.processes[0].process);
                    // On this current-thread runtime, Drop has transferred custody
                    // synchronously but its async cleanup has not yet been polled.
                    let termination_gate = process.hold_termination_for_test();
                    let shutdown_session = Arc::clone(&session);
                    let shutdown = tokio::spawn(async move {
                        shutdown_session.shutdown_runtime_for_test().await
                    });
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while !session.terminal_tasks.is_closed() {
                            assert!(!shutdown.is_finished(), "shutdown must reach the cleanup barrier");
                            tokio::task::yield_now().await;
                        }
                    }).await.expect("normal shutdown reaches tracked cleanup");
                    assert!(!shutdown.is_finished(), "normal shutdown cannot discard pending termination");
                    assert!(!session.terminal_tasks.is_empty());
                    assert_eq!(manager.pending_cleanup_owners.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner).len(), 1);
                    assert_eq!(unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) }, WAIT_TIMEOUT);
                    drop(store_guard);
                    drop(termination_gate);
                    assert!(tokio::time::timeout(Duration::from_secs(15), shutdown).await
                        .expect("normal session shutdown settles").expect("shutdown owner joins"));
                    assert_eq!(unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) }, WAIT_OBJECT_0);
                    assert!(manager.pending_cleanup_owners.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner).is_empty());
                    assert!(!manager.process_store.lock().await.processes.contains_key(&process_id));
                    assert!(session.services.command_execution.running_process(process_id).await.is_none());
                });
                drop(server);
                return;
            }
            assert!(tokio::runtime::Handle::try_current().is_err());
            let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(pending)));
            drop(store_guard);
            assert!(dropped.is_ok(), "dropping a pending remote exec outside an entered runtime must not panic");
            // The transport runtime is still alive and progressing. A native
            // wait rejects cleanup that only forgets registration or sends an
            // unprocessed transport request.
            assert_eq!(unsafe { WaitForSingleObject(child.0.as_raw_handle(), 10_000) }, WAIT_OBJECT_0);
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let store = manager.process_store.lock().await;
                        let cleaned = !store.processes.contains_key(&process_id)
                            && !store.reserved_process_ids.contains(&process_id);
                        drop(store);
                        if cleaned { break; }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await.expect("normal cleanup releases the reservation");
                assert!(session.services.command_execution.running_process(process_id).await.is_none());
            });
            drop(server);
        }).expect("test thread").join().expect("pending remote drop behavior");
}

#[cfg(windows)]
#[test]
fn remote_start_cancellation_terminates_native_child_before_start_response() {
    for cancel_token in [true, false] {
        std::thread::Builder::new()
        .name("remote-pre-start-cancel".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            use futures::SinkExt;
            use futures::StreamExt;
            use std::os::windows::io::AsRawHandle;
            use std::os::windows::io::FromRawHandle;
            use std::os::windows::io::OwnedHandle;
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
            use windows_sys::Win32::System::Threading::OpenProcess;
            use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
            use windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
            use windows_sys::Win32::System::Threading::TerminateProcess;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;

            // Own an independent native handle, including failure cleanup, so
            // a missing remote termination cannot leak the test child.
            struct ChildGuard(OwnedHandle);
            impl Drop for ChildGuard {
                fn drop(&mut self) {
                    unsafe {
                        if WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_TIMEOUT {
                            TerminateProcess(self.0.as_raw_handle(), 1);
                            WaitForSingleObject(self.0.as_raw_handle(), 5_000);
                        }
                    }
                }
            }
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("live transport runtime");
            let fixture = tempfile::tempdir().expect("child fixture");
            let marker = fixture.path().join("native-pid.txt");
            let (session, mut turn, events) = runtime.block_on(
                crate::session::tests::make_session_and_context_with_rx(),
            );
            let turn_mut = Arc::get_mut(&mut turn).expect("unique turn fixture");
            turn_mut.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
            turn_mut.approval_policy.set(codex_protocol::protocol::AskForApproval::Never)
                .expect("unrestricted fixture approval policy");
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve server address");
            let address = probe.local_addr().expect("server address");
            drop(probe);
            let url = format!("ws://{address}");
            let server_url = url.clone();
            let server = tokio_util::task::AbortOnDropHandle::new(runtime.spawn(async move {
                // Unrestricted commands do not invoke sandbox helper modes.
                let paths = codex_exec_server::ExecServerRuntimePaths::new(
                    std::env::current_exe().expect("test executable path"),
                ).expect("absolute executable path");
                codex_exec_server::run_main(&server_url, paths).await
                    .expect("real exec server");
            }));
            let (remote, proxy, start_held) = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if tokio::net::TcpStream::connect(address).await.is_ok() { break; }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let proxy_url = format!("ws://{}", listener.local_addr().unwrap());
                    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
                    // A transparent transport scheduling gate: real process/start
                    // reaches the real server, and every cleanup message passes.
                    // Withhold only its successful reply before Backend.start returns.
                    let proxy = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                        let (stream, _) = listener.accept().await.unwrap();
                        let mut downstream = tokio_tungstenite::accept_async(stream).await.unwrap();
                        let (mut upstream, _) = tokio_tungstenite::connect_async(url).await.unwrap();
                        let mut start_id = None;
                        let mut held_tx = Some(held_tx);
                        loop {
                            tokio::select! {
                                frame = downstream.next() => {
                                    let Some(Ok(frame)) = frame else { break; };
                                    if let tokio_tungstenite::tungstenite::Message::Text(text) = &frame {
                                        let value: serde_json::Value = serde_json::from_str(text).unwrap();
                                        if value["method"] == "process/start" { start_id = Some(value["id"].clone()); }
                                    }
                                    if upstream.send(frame).await.is_err() { break; }
                                }
                                frame = upstream.next() => {
                                    let Some(Ok(frame)) = frame else { break; };
                                    if let tokio_tungstenite::tungstenite::Message::Text(text) = &frame {
                                        let value: serde_json::Value = serde_json::from_str(text).unwrap();
                                        if start_id.as_ref().is_some_and(|id| value.get("id") == Some(id))
                                            && value.get("result").is_some() {
                                            held_tx.take().unwrap().send(()).unwrap();
                                            continue;
                                        }
                                    }
                                    if downstream.send(frame).await.is_err() { break; }
                                }
                            }
                        }
                    }));
                    let environment = Arc::new(codex_exec_server::Environment::create_for_tests(Some(proxy_url))
                        .expect("real remote environment through response gate"));
                    environment.wait_until_ready().await.expect("remote handshake");
                    (environment, proxy, held_rx)
                }).await.expect("exec server readiness deadline")
            });
            let manager = &session.services.unified_exec_manager;
            let reservation = runtime.block_on(manager.reserve_process_id());
            let process_id = reservation.process_id();
            let context = UnifiedExecContext::new(Arc::clone(&session), Arc::clone(&turn), "pre-start-cancel".to_string());
            let script = format!(
                "[System.IO.File]::WriteAllText('{}', [string]$PID); Start-Sleep -Seconds 60",
                marker.to_string_lossy().replace('\'', "''"),
            );
            let command = vec!["powershell.exe".to_string(), "-NoLogo".to_string(), "-NoProfile".to_string(), "-NonInteractive".to_string(), "-Command".to_string(), script.clone()];
            let attempt_key = crate::tools::command_execution::CommandAttemptKey::new(
                "exec_command", "remote", turn.cwd().to_string_lossy(), &command,
            );
            let artifact = runtime.block_on(crate::tools::command_output_artifact::create_raw_output_artifact(
                fixture.path(), "pre-start-cancel", b"",
            ));
            let request = ExecCommandRequest {
                validation: None,                command: command.clone(), command_for_safety: command,
                attempt_key, raw_output_artifact: artifact,
                shell_type: crate::shell::ShellType::PowerShell, shell_wrapper_is_owned: false,
                hook_command: script, process_id, yield_time_ms: 30_000, max_output_tokens: None,
                cwd: turn.cwd().clone().into(), normalization_cwd: None,
                sandbox_cwd: turn.cwd().clone().into(),
                turn_environment: crate::session::turn_context::TurnEnvironment::new(
                    "remote".to_string(), remote, turn.cwd().clone().into(), None,
                ),
                network: None, tty: false,
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None, additional_permissions_uri: None,
                additional_permissions_preapproved: false, justification: None, prefix_rule: None,
                validation_launch: None, known_delta: None,
            };
            let cancellation = CancellationToken::new();
            let mut pending = Box::pin(manager.exec_command(request, reservation, &context, &cancellation));
            let pid = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(15), async {
                    let observation = async {
                        start_held.await.expect("real server accepted start and proxy withheld its reply");
                        // The actual server owns the OS child, while the caller
                        // is still inside the opaque backend start await.
                        loop {
                            if let Ok(value) = tokio::fs::read_to_string(&marker).await
                                && let Ok(pid) = value.parse::<u32>() { break pid; }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    };
                    tokio::select! {
                        result = &mut pending => panic!("exec completed before withheld process/start response: {result:?}"),
                        pid = observation => pid,
                    }
                }).await.expect("registered live remote command")
            });
            let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
            assert!(!raw.is_null(), "open native child: {}", std::io::Error::last_os_error());
            let child = ChildGuard(unsafe { OwnedHandle::from_raw_handle(raw) });
            assert_eq!(unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) }, WAIT_TIMEOUT);
            runtime.block_on(async {
                assert!(manager.process_store.lock().await.processes.is_empty());
                assert!(session.services.command_execution.running_process(process_id).await.is_none());
                while let Ok(event) = events.try_recv() {
                    assert!(!matches!(event.msg, codex_protocol::protocol::EventMsg::ExecCommandBegin(_)),
                        "no locally attached process exists before the withheld start reply");
                }
            });
            if cancel_token {
                cancellation.cancel();
                let result = runtime.block_on(async {
                    tokio::time::timeout(Duration::from_secs(5), &mut pending).await
                        .expect("normal cancellation returns while start response remains withheld")
                });
                assert!(matches!(result, Err(UnifiedExecError::ProcessFailed { message }) if message == "unified exec cancelled"));
                drop(pending);
            } else {
                assert!(tokio::runtime::Handle::try_current().is_err());
                drop(pending);
            }
            // The transport runtime is still alive and progressing. A native
            // wait rejects cleanup that only forgets registration or sends an
            // unprocessed transport request.
            assert_eq!(unsafe { WaitForSingleObject(child.0.as_raw_handle(), 10_000) }, WAIT_OBJECT_0);
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let store = manager.process_store.lock().await;
                        let cleaned = !store.processes.contains_key(&process_id)
                            && !store.reserved_process_ids.contains(&process_id);
                        drop(store);
                        if cleaned { break; }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await.expect("normal cleanup releases the reservation");
                assert!(session.services.command_execution.running_process(process_id).await.is_none());
            });
            drop(proxy);
            drop(server);
        }).expect("test thread").join().expect("remote pre-start cancellation behavior");
    }
}

#[cfg(windows)]
#[test]
fn normal_local_termination_keeps_native_request_owned_after_caller_deadline() {
    use codex_tools::ToolExecutor;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    };
    struct ChildGuard(OwnedHandle);
    impl ChildGuard {
        fn exited(&self) -> bool {
            unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_OBJECT_0 }
        }
    }
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !self.exited() {
                unsafe {
                    TerminateProcess(self.0.as_raw_handle(), 1);
                    WaitForSingleObject(self.0.as_raw_handle(), 5_000);
                }
            }
        }
    }
    // Windows pipe reads use Tokio's blocking pool. Keep normal process I/O
    // on its live runtime so occupying the caller's pool isolates termination.
    let process_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("normal process runtime");
    let (session, child, process_id) = process_runtime.block_on(async {
        let fixture = tempfile::tempdir().expect("native child marker");
        let marker = fixture.path().join("pid.txt");
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
        turn.approval_policy
            .set(codex_protocol::protocol::AskForApproval::Never)
            .expect("test approval");
        let program = which::which("powershell.exe")
            .expect("Windows PowerShell")
            .to_string_lossy()
            .into_owned();
        let script = format!(
            "[System.IO.File]::WriteAllText('{}', [string]$PID); Start-Sleep -Seconds 60",
            marker.to_string_lossy().replace('\'', "''")
        );
        let args = vec![
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script,
        ];
        let mut allowed = vec![program.clone()];
        allowed.extend(args.clone());
        tokio::fs::create_dir_all(&turn.config.codex_home)
            .await
            .unwrap();
        session
            .services
            .exec_policy
            .append_amendment_and_update(
                &turn.config.codex_home,
                &codex_protocol::protocol::ExecPolicyAmendment::new(allowed),
            )
            .await
            .expect("allow the exact normal producer");
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let payload = crate::tools::context::ToolPayload::Function {
            arguments: serde_json::json!({
                "kind":"argv", "program":program, "args":args, "tty":false, "yield_time_ms":1000
            })
            .to_string(),
        };
        let output = crate::tools::handlers::ExecCommandHandler::default()
            .handle(crate::tools::context::ToolInvocation {
                session: Arc::clone(&session),
                step_context: crate::session::step_context::StepContext::for_test(Arc::clone(
                    &turn,
                )),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(tokio::sync::Mutex::new(
                    crate::turn_diff_tracker::TurnDiffTracker::new(),
                )),
                call_id: "owned-native-termination".to_string(),
                tool_name: codex_tools::ToolName::plain("exec_command"),
                source: crate::tools::context::ToolCallSource::Direct,
                payload: payload.clone(),
            })
            .await
            .expect("normal local process launch");
        let process_id = u32::try_from(
            output.code_mode_result(&payload)["session_id"]
                .as_u64()
                .expect("retained live process"),
        )
        .unwrap();
        let pid = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&marker).await
                    && let Ok(pid) = text.parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("actual native child identity");
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
        assert!(
            !raw.is_null(),
            "native handle: {}",
            std::io::Error::last_os_error()
        );
        let child = ChildGuard(unsafe { OwnedHandle::from_raw_handle(raw) });
        assert_eq!(
            unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) },
            WAIT_TIMEOUT
        );
        (session, child, process_id)
    });
    let termination_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single-worker termination caller runtime");
    termination_runtime.block_on(async {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            release_rx.recv_timeout(Duration::from_secs(5)).is_ok()
        });
        started_rx.await.unwrap();
        let terminated = tokio::time::timeout(
            Duration::from_millis(50),
            session.terminate_background_terminal(process_id),
        )
        .await;
        assert!(
            terminated.is_err(),
            "queued native request must permit caller deadline"
        );
        assert!(
            !blocker.is_finished(),
            "runtime deadline progresses while worker remains occupied"
        );
        assert!(
            !child.exited(),
            "termination must not run inline ahead of the queued worker"
        );
        assert!(
            session
                .services
                .unified_exec_manager
                .process_store
                .lock()
                .await
                .processes
                .contains_key(&process_id),
            "unconfirmed caller must retain registered process ownership"
        );
        release_tx.send(()).unwrap();
        assert!(blocker.await.unwrap());
        tokio::time::timeout(Duration::from_secs(10), async {
            while !child.exited() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the accepted worker kills the real child after caller cancellation");
        let _ = session.terminate_background_terminal(process_id).await;
        assert!(
            !session
                .services
                .unified_exec_manager
                .process_store
                .lock()
                .await
                .processes
                .contains_key(&process_id)
        );
        assert!(session.list_background_terminals().await.is_empty());
    });
    drop(session);
    drop(termination_runtime);
    drop(process_runtime);
}

#[cfg(windows)]
#[test]
fn registered_nonpty_interrupt_yields_to_worker_and_preserves_unsupported_process() {
    use codex_tools::ToolExecutor;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    };
    struct ChildGuard(OwnedHandle);
    impl ChildGuard {
        fn exited(&self) -> bool {
            unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_OBJECT_0 }
        }
    }
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !self.exited() {
                unsafe {
                    TerminateProcess(self.0.as_raw_handle(), 1);
                    WaitForSingleObject(self.0.as_raw_handle(), 5_000);
                }
            }
        }
    }
    // Windows pipe reads use Tokio's blocking pool. Keep normal process I/O
    // on its live runtime so occupying the caller's pool isolates termination.
    let process_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("normal process runtime");
    let (session, turn, child, process_id) = process_runtime.block_on(async {
        let fixture = tempfile::tempdir().expect("native child marker");
        let marker = fixture.path().join("pid.txt");
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        let mut config = (*turn.config).clone();
        config
            .features
            .enable(codex_features::Feature::UnifiedExec)
            .unwrap();
        turn.config = Arc::new(config);
        turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
        turn.approval_policy
            .set(codex_protocol::protocol::AskForApproval::Never)
            .expect("test approval");
        let program = which::which("powershell.exe")
            .expect("Windows PowerShell")
            .to_string_lossy()
            .into_owned();
        let script = format!(
            "[System.IO.File]::WriteAllText('{}', [string]$PID); Start-Sleep -Seconds 60",
            marker.to_string_lossy().replace('\'', "''")
        );
        let args = vec![
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script,
        ];
        let mut allowed = vec![program.clone()];
        allowed.extend(args.clone());
        tokio::fs::create_dir_all(&turn.config.codex_home)
            .await
            .unwrap();
        session
            .services
            .exec_policy
            .append_amendment_and_update(
                &turn.config.codex_home,
                &codex_protocol::protocol::ExecPolicyAmendment::new(allowed),
            )
            .await
            .expect("allow the exact normal producer");
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let payload = crate::tools::context::ToolPayload::Function {
            arguments: serde_json::json!({
                "kind":"argv", "program":program, "args":args, "tty":false, "yield_time_ms":1000
            })
            .to_string(),
        };
        let output = crate::tools::handlers::ExecCommandHandler::default()
            .handle(crate::tools::context::ToolInvocation {
                session: Arc::clone(&session),
                step_context: crate::session::step_context::StepContext::for_test(Arc::clone(
                    &turn,
                )),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(tokio::sync::Mutex::new(
                    crate::turn_diff_tracker::TurnDiffTracker::new(),
                )),
                call_id: "interrupt-producer".to_string(),
                tool_name: codex_tools::ToolName::plain("exec_command"),
                source: crate::tools::context::ToolCallSource::Direct,
                payload: payload.clone(),
            })
            .await
            .expect("normal local process launch");
        let process_id = u32::try_from(
            output.code_mode_result(&payload)["session_id"]
                .as_u64()
                .expect("retained live process"),
        )
        .unwrap();
        let pid = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&marker).await
                    && let Ok(pid) = text.parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("actual native child identity");
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
        assert!(
            !raw.is_null(),
            "native handle: {}",
            std::io::Error::last_os_error()
        );
        let child = ChildGuard(unsafe { OwnedHandle::from_raw_handle(raw) });
        assert_eq!(
            unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) },
            WAIT_TIMEOUT
        );
        (session, turn, child, process_id)
    });
    let caller_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single worker interrupt runtime");
    caller_runtime.block_on(async {
        let step = crate::session::step_context::StepContext::for_test(turn);
        let router = Arc::new(crate::tools::router::ToolRouter::from_context(
            step.as_ref(),
            crate::tools::router::ToolRouterParams {
                tool_suggest_candidates: None,
                deferred_mcp_tools: None,
                mcp_tools: None,
                extension_tool_executors: Vec::new(),
                dynamic_tools: &[],
                exposure_identity: Default::default(),
            },
            &Default::default(),
        ));
        assert!(step.set_tool_router(router).is_ok());
        let runtime = crate::tools::parallel::ToolCallRuntime::new(
            Arc::clone(&session), Arc::clone(&step),
            Arc::new(tokio::sync::Mutex::new(crate::turn_diff_tracker::TurnDiffTracker::new())),
        );
        assert!(runtime.has_registered_tool(&codex_tools::ToolName::plain("write_stdin")));
        let process = Arc::clone(
            &session.services.unified_exec_manager.process_store.lock().await
                .processes.get(&process_id).expect("normal registered process").process,
        );
        assert!(!process.termination_was_requested());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            release_rx.recv_timeout(Duration::from_secs(5)).is_ok()
        });
        started_rx.await.unwrap();
        let payload = crate::tools::context::ToolPayload::Function {
            arguments: serde_json::json!({
                "session_id": process_id, "chars": "\u{3}", "yield_time_ms": 1000,
            }).to_string(),
        };
        // Exercise the normal handler directly to isolate native signal work
        // from the registered pipeline's earlier worker-backed preflight.
        let handler = crate::tools::handlers::WriteStdinHandler;
        let mut interrupt = handler.handle(crate::tools::context::ToolInvocation {
            session: Arc::clone(&session),
            step_context: Arc::clone(&step),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(crate::turn_diff_tracker::TurnDiffTracker::new())),
            call_id: "handler-native-interrupt".to_string(),
            tool_name: codex_tools::ToolName::plain("write_stdin"),
            source: crate::tools::context::ToolCallSource::Direct,
            payload: payload.clone(),
        });
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut interrupt).await.is_err(),
            "native signal must queue behind the occupied worker, not run on the async caller");
        assert!(process.termination_was_requested(),
            "normal handler must reach the actual interrupt boundary before the deadline");
        assert!(process.interaction_lock().try_lock().is_err(),
            "native interrupt is still pending inside write_stdin, not finished with registry persistence pending");
        assert!(!blocker.is_finished(), "caller timer progresses while worker remains occupied");
        assert!(!child.exited(), "queued interrupt must preserve the actual native process");
        assert!(session.services.unified_exec_manager.process_store.lock().await.processes.contains_key(&process_id));
        release_tx.send(()).unwrap();
        assert!(blocker.await.unwrap());
        let native_result = tokio::time::timeout(Duration::from_secs(10), interrupt).await
            .expect("released worker returns real native outcome");
        assert!(matches!(native_result,
            Err(crate::FunctionCallError::RespondToModel(message))
            if message.contains("process interrupt is not supported by this process backend")));
        assert!(!child.exited(), "unsupported native signal preserves the live process");
        // The registered path independently proves the same native error is
        // exposed through the actual model-facing tool registration.
        let response = tokio::time::timeout(Duration::from_secs(10), runtime.handle_tool_call(
            crate::tools::router::ToolCall {
                tool_name: codex_tools::ToolName::plain("write_stdin"),
                call_id: "registered-native-interrupt".to_string(),
                payload,
            },
            CancellationToken::new(),
        )).await.expect("registered interrupt settles").expect("normal registered output");
        let codex_protocol::models::ResponseInputItem::FunctionCallOutput { call_id, output } = response else {
            panic!("expected function output");
        };
        assert_eq!(call_id, "registered-native-interrupt");
        assert_eq!(output.success, Some(false));
        let text = output.body.to_text().expect("model-visible interrupt error");
        assert!(text.contains("process interrupt is not supported by this process backend"), "{text}");
        assert!(!child.exited(), "unsupported signal must not become termination");
        assert!(session.services.unified_exec_manager.process_store.lock().await.processes.contains_key(&process_id),
            "failed interrupt retains the normal registered process");
        assert!(session.services.command_execution.running_process(process_id).await.is_some(),
            "failed interrupt must not claim command completion");
    });
    process_runtime.block_on(async {
        assert!(session.terminate_background_terminal(process_id).await);
        assert!(child.exited(), "normal cleanup confirms real child exit");
        assert!(session.list_background_terminals().await.is_empty());
    });
    drop(session);
    drop(caller_runtime);
    drop(process_runtime);
}

#[cfg(windows)]
#[test]
fn remote_commit_retirement_yields_and_cancellation_cleans_registered_child() {
    for cancel_token in [true, false] {
        std::thread::Builder::new()
        .name("remote-commit-retirement".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            use std::os::windows::io::AsRawHandle;
            use std::os::windows::io::FromRawHandle;
            use std::os::windows::io::OwnedHandle;
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
            use windows_sys::Win32::System::Threading::OpenProcess;
            use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
            use windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
            use windows_sys::Win32::System::Threading::TerminateProcess;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;

            // Own an independent native handle, including failure cleanup, so
            // a missing remote termination cannot leak the test child.
            struct ChildGuard(OwnedHandle);
            impl Drop for ChildGuard {
                fn drop(&mut self) {
                    unsafe {
                        if WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_TIMEOUT {
                            TerminateProcess(self.0.as_raw_handle(), 1);
                            WaitForSingleObject(self.0.as_raw_handle(), 5_000);
                        }
                    }
                }
            }
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("live transport runtime");
            let fixture = tempfile::tempdir().expect("child fixture");
            let marker = fixture.path().join("native-pid.txt");
            let (session, mut turn, events) = runtime.block_on(
                crate::session::tests::make_session_and_context_with_rx(),
            );
            let turn_mut = Arc::get_mut(&mut turn).expect("unique turn fixture");
            turn_mut.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
            turn_mut.approval_policy.set(codex_protocol::protocol::AskForApproval::Never)
                .expect("unrestricted fixture approval policy");
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve server address");
            let address = probe.local_addr().expect("server address");
            drop(probe);
            let url = format!("ws://{address}");
            let server_url = url.clone();
            let server = tokio_util::task::AbortOnDropHandle::new(runtime.spawn(async move {
                // Unrestricted commands do not invoke sandbox helper modes.
                let paths = codex_exec_server::ExecServerRuntimePaths::new(
                    std::env::current_exe().expect("test executable path"),
                ).expect("absolute executable path");
                codex_exec_server::run_main(&server_url, paths).await
                    .expect("real exec server");
            }));
            let remote = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if tokio::net::TcpStream::connect(address).await.is_ok() { break; }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    let environment = Arc::new(codex_exec_server::Environment::create_for_tests(Some(url))
                        .expect("real remote environment"));
                    environment.wait_until_ready().await.expect("remote handshake");
                    environment
                }).await.expect("exec server readiness deadline")
            });
            let manager = &session.services.unified_exec_manager;
            let reservation = runtime.block_on(manager.reserve_process_id());
            let process_id = reservation.process_id();
            let context = UnifiedExecContext::new(Arc::clone(&session), Arc::clone(&turn), "commit-retirement".to_string());
            let script = format!(
                "[System.IO.File]::WriteAllText('{}', [string]$PID); Start-Sleep -Seconds 60",
                marker.to_string_lossy().replace('\'', "''"),
            );
            let command = vec!["powershell.exe".to_string(), "-NoLogo".to_string(), "-NoProfile".to_string(), "-NonInteractive".to_string(), "-Command".to_string(), script.clone()];
            let attempt_key = crate::tools::command_execution::CommandAttemptKey::new(
                "exec_command", "remote", turn.cwd().to_string_lossy(), &command,
            );
            let artifact = runtime.block_on(crate::tools::command_output_artifact::create_raw_output_artifact(
                fixture.path(), "commit-retirement", b"",
            ));
            let request = ExecCommandRequest {
                validation: None,                command: command.clone(), command_for_safety: command,
                attempt_key, raw_output_artifact: artifact,
                shell_type: crate::shell::ShellType::PowerShell, shell_wrapper_is_owned: false,
                hook_command: script, process_id, yield_time_ms: 30_000, max_output_tokens: None,
                cwd: turn.cwd().clone().into(), normalization_cwd: None,
                sandbox_cwd: turn.cwd().clone().into(),
                turn_environment: crate::session::turn_context::TurnEnvironment::new(
                    "remote".to_string(), remote, turn.cwd().clone().into(), None,
                ),
                network: None, tty: false,
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None, additional_permissions_uri: None,
                additional_permissions_preapproved: false, justification: None, prefix_rule: None,
                validation_launch: None, known_delta: None,
            };
            let cancellation = CancellationToken::new();
            // Scheduling control only: the normal execution must attach the
            // remote process, then wait here before store insertion/commit.
            let store_guard = runtime.block_on(manager.process_store.lock());
            let mut pending = Box::pin(manager.exec_command(request, reservation, &context, &cancellation));
            let pid = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(15), async {
                    let observation = async {
                        loop {
                            let event = events.recv().await.expect("normal command event");
                            if let codex_protocol::protocol::EventMsg::ExecCommandBegin(begin) = event.msg {
                                assert_eq!(begin.call_id, "commit-retirement");
                                break;
                            }
                        }
                        // ExecCommandBegin is emitted only after attach_process;
                        // the PID marker independently identifies the OS child.
                        loop {
                            if let Ok(value) = tokio::fs::read_to_string(&marker).await
                                && let Ok(pid) = value.parse::<u32>() { break pid; }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    };
                    tokio::select! {
                        result = &mut pending => panic!("exec completed before pending-registration barrier: {result:?}"),
                        pid = observation => pid,
                    }
                }).await.expect("registered live remote command")
            });
            let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
            assert!(!raw.is_null(), "open native child: {}", std::io::Error::last_os_error());
            let child = ChildGuard(unsafe { OwnedHandle::from_raw_handle(raw) });
            assert_eq!(unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) }, WAIT_TIMEOUT);
            let caller_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all().max_blocking_threads(2).build().expect("retirement caller runtime");
            caller_runtime.block_on(async {
                // Hold only the actual retirement worker. The other worker
                // remains available for the legitimate exit emitter/analysis.
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let (taken_tx, taken_rx) = tokio::sync::oneshot::channel();
                let (retired_tx, mut retired_rx) = tokio::sync::oneshot::channel();
                crate::unified_exec::PENDING_SPAWN_RETIREMENT_OBSERVER.with(|observer| {
                    assert!(observer.borrow_mut().replace((taken_tx, retired_tx, release_rx)).is_none());
                });
                drop(store_guard);
                let taken = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::select! {
                        result = &mut pending => panic!("normal command completed before pending-owner retirement: {result:?}"),
                        count = taken_rx => count.expect("normal commit reaches pending-owner retirement"),
                    }
                }).await.expect("normal commit reaches its exact retirement boundary");
                assert_eq!(taken.len(), 1);
                let retired_process = &taken[0];
                assert!(tokio::time::timeout(Duration::from_millis(50), &mut pending).await.is_err());
                assert!(matches!(retired_rx.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
                    "the actual pending vector remains owned by held retirement");
                assert_eq!(unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) }, WAIT_TIMEOUT);
                let store = tokio::time::timeout(Duration::from_millis(50), manager.process_store.lock())
                    .await.expect("retirement does not retain the store mutex");
                assert!(store.processes.contains_key(&process_id), "commit retains normal process custody");
                drop(store);
                assert!(session.services.command_execution.running_process(process_id).await.is_some());
                if cancel_token {
                    cancellation.cancel();
                    let result = tokio::time::timeout(Duration::from_secs(5), &mut pending).await
                        .expect("normal cancellation settles independently of held retirement");
                    assert!(matches!(result, Err(UnifiedExecError::ProcessFailed { message }) if message == "unified exec cancelled"));
                    drop(pending);
                } else {
                    drop(pending);
                    let store = manager.process_store.lock().await;
                    let entry = store.processes.get(&process_id).expect("hard drop retains committed session ownership");
                    assert!(!entry.initial_exec_command_active.load(Ordering::Acquire),
                        "initial command guard must cover the new commit retirement await");
                    drop(store);
                    assert!(session.services.command_execution.running_process(process_id).await.is_some());
                    assert_eq!(unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) }, WAIT_TIMEOUT);
                    assert!(session.terminate_background_terminal(process_id).await,
                        "normal session termination cleans the committed process after hard drop");
                }
                tokio::time::timeout(Duration::from_secs(5), async {
                    while retired_process.strong_count() != 1 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }).await.expect("retirement worker retains the final process owner after normal cancellation");
                assert!(matches!(retired_rx.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
                    "cancelling the awaiting command does not discard the held retirement owner");
                // The registered command is now cancelled and its other owners
                // are gone. Only the held retirement keeps this object alive.
                release_tx.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(5), retired_rx).await
                    .expect("accepted retirement survives caller cancellation").expect("worker retires owned vector");
                assert!(retired_process.upgrade().is_none(), "worker retires the actual final process owner");
                assert_eq!(unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) }, WAIT_OBJECT_0,
                    "normal cancellation confirms real remote child exit");
                let store = manager.process_store.lock().await;
                assert!(!store.processes.contains_key(&process_id));
                assert!(!store.reserved_process_ids.contains(&process_id));
                drop(store);
                assert!(session.services.command_execution.running_process(process_id).await.is_none());
            });
            drop(caller_runtime);
            drop(server);
        }).expect("test thread").join().expect("normal remote retirement cancellation behavior");
    }
}
