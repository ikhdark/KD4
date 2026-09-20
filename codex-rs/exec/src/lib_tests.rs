use super::*;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::trace::TraceId;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use pretty_assertions::assert_eq;
use std::io;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use tempfile::tempdir;
use tracing_opentelemetry::OpenTelemetrySpanExt;

pub(crate) fn recovery_completion() -> ServerNotification {
    let mut turn = recovery_thread_fixture().turns.remove(0);
    turn.items.clear();
    ServerNotification::TurnCompleted(codex_app_server_protocol::TurnCompletedNotification {
        thread_id: "thread-1".into(),
        turn,
        surfaced_result: None,
        timing: None,
    })
}

#[tokio::test]
async fn notification_preparation_filters_before_reading_history() {
    let mut notification = recovery_completion();
    let result = prepare_server_notification(
        false,
        "other-thread",
        "turn-1",
        false,
        &mut notification,
        async |_| panic!("foreign completions must not read history"),
    )
    .await;
    assert_eq!(result, Ok(false));
    let result = prepare_server_notification(
        false,
        "thread-1",
        "other-turn",
        false,
        &mut notification,
        async |_| panic!("foreign turns must not read history"),
    )
    .await;
    assert_eq!(result, Ok(false));
    let result = prepare_server_notification(
        false,
        "thread-1",
        "turn-1",
        false,
        &mut notification,
        async |id| {
            assert_eq!(id, "thread-1");
            Ok(ThreadReadResponse {
                thread: recovery_thread_fixture(),
            })
        },
    )
    .await;
    assert_eq!(result, Ok(true));
    let ServerNotification::TurnCompleted(payload) = notification else {
        panic!("completion")
    };
    assert_eq!(
        payload.turn.items,
        recovery_thread_fixture().turns.remove(0).items
    );
}

#[tokio::test]
async fn required_recovery_failure_preserves_last_message_artifact() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("answer.txt");
    std::fs::write(&path, "previous answer").expect("seed artifact");
    let mut processor = EventProcessorWithJsonOutput::new(Some(path.clone()));
    processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            thread_id: "thread-1".into(),
            turn_id: "turn-1".into(),
            completed_at_ms: 0,
            item: AppServerThreadItem::AgentMessage {
                id: "early".into(),
                text: "commentary".into(),
                phase: Some(MessagePhase::Commentary),
                memory_citation: None,
            },
        },
    ));
    let mut notification = recovery_completion();
    let error = prepare_server_notification(
        false,
        "thread-1",
        "turn-1",
        false,
        &mut notification,
        async |_| Err("history unavailable".into()),
    )
    .await
    .expect_err("required recovery must fail");
    assert!(error.contains("history unavailable"));
    processor.collect_event_stream_error(error);
    processor.print_final_output().expect("preserve artifact");
    assert_eq!(processor.final_message(), None);
    assert_eq!(
        std::fs::read_to_string(path).expect("read artifact"),
        "previous answer"
    );
}

#[tokio::test]
async fn recovery_failure_preserves_authoritative_results_but_missing_turn_is_an_error() {
    let mut notification = recovery_completion();
    assert_eq!(
        prepare_server_notification(
            false,
            "thread-1",
            "turn-1",
            true,
            &mut notification,
            async |_| Err("history unavailable".into())
        )
        .await,
        Ok(true)
    );
    let missing_turn = prepare_server_notification(
        false,
        "thread-1",
        "turn-1",
        false,
        &mut notification,
        async |_| {
            let mut thread = recovery_thread_fixture();
            thread.turns.clear();
            Ok(ThreadReadResponse { thread })
        },
    )
    .await
    .expect_err("missing requested turn is not successful recovery");
    assert!(missing_turn.contains("did not contain completed turn turn-1"));
    let ServerNotification::TurnCompleted(payload) = &mut notification else {
        panic!("completion")
    };
    payload.surfaced_result = Some(codex_protocol::protocol::SurfacedToolResult {
        adapter: "owner".into(),
        value: serde_json::json!({"answer":42}),
        canonical_message: None,
    });
    assert_eq!(
        prepare_server_notification(
            false,
            "thread-1",
            "turn-1",
            false,
            &mut notification,
            async |_| Err("history unavailable".into())
        )
        .await,
        Ok(true)
    );
}

#[tokio::test]
async fn latest_cwd_reads_across_chunks_and_skips_malformed_tail() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    let old_cwd = dir.path().join("old");
    let latest_cwd = dir.path().join("latest-新");
    let context = |cwd: &Path| {
        serde_json::json!({
        "timestamp":"2026-09-13T00:00:00Z", "type":"turn_context", "payload": {
            "cwd":cwd, "approval_policy":"never", "sandbox_policy":{"type":"danger-full-access"},
            "model":"gpt-5", "summary":"auto"
        }
    }).to_string()
    };
    std::fs::write(
        &path,
        format!(
            "{}\r\n{}\r\n{}\n{{broken tail",
            context(&old_cwd),
            context(&latest_cwd),
            "x".repeat(20000)
        ),
    )
    .expect("write history");
    assert_eq!(parse_latest_turn_context_cwd(&path).await, Some(latest_cwd));
    std::fs::write(&path, context(&old_cwd)).expect("write first record without newline");
    assert_eq!(parse_latest_turn_context_cwd(&path).await, Some(old_cwd));
    std::fs::write(&path, "invalid\n").expect("invalid history");
    assert_eq!(parse_latest_turn_context_cwd(&path).await, None);
}

fn test_tracing_subscriber() -> impl tracing::Subscriber + Send + Sync {
    let provider = SdkTracerProvider::builder().build();
    let tracer = provider.tracer("codex-exec-tests");
    tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer))
}

#[tokio::test]
async fn exec_defaults_analytics_to_enabled() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build default config");
    config.analytics_enabled = None;
    config.otel.exporter = codex_config::types::OtelExporterKind::None;
    config.otel.trace_exporter = codex_config::types::OtelExporterKind::None;
    config.otel.metrics_exporter = codex_config::types::OtelExporterKind::OtlpGrpc {
        endpoint: "http://127.0.0.1:1".to_string(),
        headers: Default::default(),
        tls: None,
    };

    let provider = build_exec_otel_provider(&config)
        .expect("build metrics provider")
        .expect("exec enables the configured metrics exporter by default");
    assert!(provider.metrics().is_some());
    provider.shutdown();
    config.analytics_enabled = Some(false);
    assert!(
        build_exec_otel_provider(&config)
            .expect("build explicitly disabled metrics provider")
            .is_none(),
        "explicit analytics opt-out must prevent exporter initialization"
    );
}

#[derive(Clone)]
struct TestLogWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

struct TestLogSink {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TestLogWriter {
    type Writer = TestLogSink;

    fn make_writer(&'a self) -> Self::Writer {
        TestLogSink {
            buffer: Arc::clone(&self.buffer),
        }
    }
}

impl Write for TestLogSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.lock().expect("log buffer lock").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn exec_default_stderr_filter_suppresses_otel_self_diagnostics() {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = TestLogWriter {
        buffer: Arc::clone(&buffer),
    };
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer)
            .with_filter(EnvFilter::try_new(EXEC_DEFAULT_LOG_FILTER).expect("default filter")),
    );

    tracing::subscriber::with_default(subscriber, || {
        tracing::error!(target: "opentelemetry_sdk", "telemetry export failed");
        tracing::error!(target: "opentelemetry_otlp", "telemetry request failed");
        tracing::error!(target: "codex_exec_test", "real exec error");
    });

    let logs = String::from_utf8(buffer.lock().expect("log buffer lock").clone()).expect("utf8");
    assert!(!logs.contains("telemetry export failed"));
    assert!(!logs.contains("telemetry request failed"));
    assert!(logs.contains("real exec error"));
}

#[test]
fn exec_root_span_can_be_parented_from_trace_context() {
    let subscriber = test_tracing_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let parent = codex_protocol::protocol::W3cTraceContext {
        traceparent: Some("00-00000000000000000000000000000077-0000000000000088-01".into()),
        tracestate: Some("vendor=value".into()),
    };
    let exec_span = exec_root_span();
    assert!(set_parent_from_w3c_trace_context(&exec_span, &parent));

    let trace_id = exec_span.context().span().span_context().trace_id();
    assert_eq!(
        trace_id,
        TraceId::from_hex("00000000000000000000000000000077").expect("trace id")
    );
}

#[tokio::test]
async fn review_rejects_before_configuration_is_loaded() {
    use clap::Parser;
    let mut cli = Cli::parse_from(["codex-exec", "review", "--uncommitted"]);
    cli.config_overrides
        .raw_overrides
        .push("not a valid override".to_string());
    let error = run_main(cli, Arg0DispatchPaths::default())
        .await
        .expect_err("review is unavailable");
    assert_eq!(
        error.to_string(),
        "review requests are not available through the app-server protocol"
    );
}

#[test]
fn decode_prompt_bytes_strips_utf8_bom() {
    let input = [0xEF, 0xBB, 0xBF, b'h', b'i', b'\n'];

    let out = decode_prompt_bytes(&input).expect("decode utf-8 with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_decodes_utf16le_bom() {
    // UTF-16LE BOM + "hi\n"
    let input = [0xFF, 0xFE, b'h', 0x00, b'i', 0x00, b'\n', 0x00];

    let out = decode_prompt_bytes(&input).expect("decode utf-16le with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_decodes_utf16be_bom() {
    // UTF-16BE BOM + "hi\n"
    let input = [0xFE, 0xFF, 0x00, b'h', 0x00, b'i', 0x00, b'\n'];

    let out = decode_prompt_bytes(&input).expect("decode utf-16be with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_rejects_utf32le_bom() {
    // UTF-32LE BOM + "hi\n"
    let input = [
        0xFF, 0xFE, 0x00, 0x00, b'h', 0x00, 0x00, 0x00, b'i', 0x00, 0x00, 0x00, b'\n', 0x00, 0x00,
        0x00,
    ];

    let err = decode_prompt_bytes(&input).expect_err("utf-32le should be rejected");

    assert_eq!(
        err,
        PromptDecodeError::UnsupportedBom {
            encoding: "UTF-32LE"
        }
    );
}

#[test]
fn decode_prompt_bytes_rejects_utf32be_bom() {
    // UTF-32BE BOM + "hi\n"
    let input = [
        0x00, 0x00, 0xFE, 0xFF, 0x00, 0x00, 0x00, b'h', 0x00, 0x00, 0x00, b'i', 0x00, 0x00, 0x00,
        b'\n',
    ];

    let err = decode_prompt_bytes(&input).expect_err("utf-32be should be rejected");

    assert_eq!(
        err,
        PromptDecodeError::UnsupportedBom {
            encoding: "UTF-32BE"
        }
    );
}

#[test]
fn decode_prompt_bytes_rejects_invalid_utf8() {
    // Invalid UTF-8 sequence: 0xC3 0x28
    let input = [0xC3, 0x28];

    let err = decode_prompt_bytes(&input).expect_err("invalid utf-8 should fail");

    assert_eq!(err, PromptDecodeError::InvalidUtf8 { valid_up_to: 0 });
}

#[test]
fn prompt_with_stdin_context_wraps_stdin_with_one_trailing_newline() {
    for input in ["my output", "my output\n"] {
        assert_eq!(
            prompt_with_stdin_context("Summarize this concisely", input),
            "Summarize this concisely\n\n<stdin>\nmy output\n</stdin>",
            "stdin: {input:?}"
        );
    }
}

#[test]
fn lagged_event_warning_message_is_explicit() {
    assert_eq!(
        lagged_event_warning_message(/*skipped*/ 7),
        "in-process app-server event stream lagged; dropped 7 events".to_string()
    );
}

#[test]
fn runtime_warnings_are_filtered_to_the_primary_thread() {
    let primary_thread_id = "thread-1";
    let turn_id = "turn-1";
    let outcomes = [
        codex_app_server_protocol::WarningNotification {
            thread_id: None,
            message: "global warning".to_string(),
        },
        codex_app_server_protocol::WarningNotification {
            thread_id: Some(primary_thread_id.to_string()),
            message: "primary warning".to_string(),
        },
        codex_app_server_protocol::WarningNotification {
            thread_id: Some("thread-2".to_string()),
            message: "other warning".to_string(),
        },
    ]
    .map(|warning| {
        should_process_notification(
            &ServerNotification::Warning(warning),
            primary_thread_id,
            turn_id,
        )
    });

    assert_eq!(outcomes, [true, true, false]);
}

#[tokio::test]
async fn resume_lookup_model_providers_filters_only_last_lookup() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build default config");
    config.model_provider_id = "test-provider".to_string();

    let last_args = crate::cli::ResumeArgs {
        session_id: None,
        last: true,
        all: false,
        images: vec![],
        prompt: None,
    };
    let named_args = crate::cli::ResumeArgs {
        session_id: Some("named-session".to_string()),
        last: false,
        all: false,
        images: vec![],
        prompt: None,
    };

    assert_eq!(
        resume_lookup_model_providers(&config, &last_args),
        Some(vec!["test-provider".to_string()])
    );
    assert_eq!(resume_lookup_model_providers(&config, &named_args), None);
}

fn recovery_thread_fixture() -> AppServerThread {
    AppServerThread {
        project_id: None,
        id: "thread-1".to_string(),
        extra: None,
        session_id: "thread-1".to_string(),
        forked_from_id: None,
        parent_thread_id: None,
        preview: String::new(),
        ephemeral: false,
        history_mode: Default::default(),
        model_provider: "openai".to_string(),
        created_at: 0,
        updated_at: 0,
        recency_at: Some(0),
        status: codex_app_server_protocol::ThreadStatus::Idle,
        path: None,
        cwd: test_path_buf("/tmp/project").abs(),
        cli_version: "0.0.0-test".to_string(),
        source: codex_app_server_protocol::SessionSource::Exec,
        thread_source: None,
        agent_nickname: None,
        agent_role: None,
        git_info: None,
        name: None,
        turns: vec![
            codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![AppServerThreadItem::AgentMessage {
                    id: "msg-1".to_string(),
                    text: "hello".to_string(),
                    phase: None,
                    memory_citation: None,
                }],
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            },
            codex_app_server_protocol::Turn {
                id: "turn-2".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![AppServerThreadItem::Plan {
                    id: "plan-1".to_string(),
                    text: "ship it".to_string(),
                }],
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            },
        ],
    }
}

#[test]
fn turn_items_for_thread_returns_matching_turn_items() {
    let thread = recovery_thread_fixture();

    assert_eq!(
        turn_items_for_thread(thread.clone(), "turn-1"),
        Some(vec![AppServerThreadItem::AgentMessage {
            id: "msg-1".to_string(),
            text: "hello".to_string(),
            phase: None,
            memory_citation: None,
        }])
    );
    assert_eq!(turn_items_for_thread(thread, "missing-turn"), None);
}

#[test]
fn should_backfill_turn_completed_items_requires_missing_persisted_items() {
    let mut notification =
        ServerNotification::TurnCompleted(codex_app_server_protocol::TurnCompletedNotification {
            surfaced_result: None,
            thread_id: "thread-1".to_string(),
            timing: None,
            turn: codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            },
        });

    assert!(!should_backfill_turn_completed_items(
        /*thread_ephemeral*/ true,
        &notification
    ));
    assert!(should_backfill_turn_completed_items(false, &notification));
    let ServerNotification::TurnCompleted(payload) = &mut notification else {
        panic!("completion")
    };
    payload.turn.items.push(AppServerThreadItem::Plan {
        id: "plan".into(),
        text: "plan".into(),
    });
    assert!(!should_backfill_turn_completed_items(false, &notification));
}

#[test]
fn canceled_mcp_server_elicitation_response_uses_cancel_action() {
    let value = canceled_mcp_server_elicitation_response()
        .expect("mcp elicitation cancel response should serialize");
    let response: McpServerElicitationRequestResponse =
        serde_json::from_value(value).expect("cancel response should deserialize");

    assert_eq!(
        response,
        McpServerElicitationRequestResponse {
            action: McpServerElicitationAction::Cancel,
            content: None,
            meta: None,
        }
    );
}

#[tokio::test]
async fn thread_start_params_preserve_configured_permissions() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with default permissions");

    let params = thread_start_params_from_config(&config);

    assert_eq!(params.sandbox, None);
    assert_eq!(
        params.permissions,
        permissions_selection_from_config(&config)
    );
}

#[tokio::test]
async fn headless_approval_policy_applies() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            headless_approval_policy: Some(AskForApproval::Never),
            ..Default::default()
        })
        .build()
        .await
        .expect("headless config should apply the headless policy");

    assert_eq!(
        config.permissions.approval_policy.value(),
        AskForApproval::Never
    );
}

#[tokio::test]
async fn thread_start_params_include_user_thread_source() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");

    let params = thread_start_params_from_config(&config);

    assert_eq!(
        params.thread_source,
        Some(codex_app_server_protocol::ThreadSource::User)
    );
}

#[tokio::test]
async fn thread_lifecycle_params_preserve_hook_trust_bypass() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            bypass_hook_trust: Some(true),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with hook trust bypass");
    let start_params = thread_start_params_from_config(&config);
    let resume_params = thread_resume_params_from_config(&config, "thread-id".to_string());

    assert_eq!(start_params.config, resume_params.config);
    assert_eq!(
        start_params
            .config
            .as_ref()
            .and_then(|config| config.get("bypass_hook_trust")),
        Some(&serde_json::Value::Bool(true))
    );
}

#[test]
fn active_profile_selection_uses_profile_id_only() {
    let selection = permission_profile_id_from_active_profile(ActivePermissionProfile::new(
        BUILT_IN_PERMISSION_PROFILE_WORKSPACE,
    ));

    assert_eq!(selection, BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string());
}

#[tokio::test]
async fn thread_lifecycle_params_include_legacy_sandbox_when_no_active_profile() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            sandbox_mode: Some(SandboxMode::DangerFullAccess),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with legacy sandbox override");

    let start_params = thread_start_params_from_config(&config);
    let resume_params = thread_resume_params_from_config(&config, "thread-id".to_string());

    assert_eq!(config.permissions.active_permission_profile(), None);
    assert_eq!(
        start_params.sandbox,
        Some(codex_app_server_protocol::SandboxMode::DangerFullAccess)
    );
    assert_eq!(start_params.permissions, None);
    assert_eq!(
        resume_params.sandbox,
        Some(codex_app_server_protocol::SandboxMode::DangerFullAccess)
    );
    assert_eq!(resume_params.permissions, None);
}

#[tokio::test]
async fn session_configured_from_thread_response_preserves_session_contract() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");
    let parent_thread_id = ThreadId::new();
    let mut response = sample_thread_start_response();
    response.thread.parent_thread_id = Some(parent_thread_id.to_string());

    let event = session_configured_from_thread_start_response(&response, &config)
        .expect("build bootstrap session configured event");

    assert_eq!(
        event.session_id.to_string(),
        "67e55044-10b1-426f-9247-bb680e5fe0c7"
    );
    assert_eq!(
        event.thread_id.to_string(),
        "67e55044-10b1-426f-9247-bb680e5fe0c8"
    );
    assert_eq!(
        event.permission_profile,
        config.permissions.effective_permission_profile()
    );
    assert_eq!(
        event.thread_source,
        Some(codex_protocol::protocol::ThreadSource::User)
    );
    assert_eq!(event.parent_thread_id, Some(parent_thread_id));
}

fn sample_thread_start_response() -> ThreadStartResponse {
    ThreadStartResponse {
        thread: codex_app_server_protocol::Thread {
            project_id: None,
            id: "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string(),
            extra: None,
            session_id: "67e55044-10b1-426f-9247-bb680e5fe0c7".to_string(),
            forked_from_id: None,
            parent_thread_id: None,
            preview: String::new(),
            ephemeral: false,
            history_mode: Default::default(),
            model_provider: "openai".to_string(),
            created_at: 0,
            updated_at: 0,
            recency_at: Some(0),
            status: codex_app_server_protocol::ThreadStatus::Idle,
            path: Some(PathBuf::from("/tmp/rollout.jsonl")),
            cwd: test_path_buf("/tmp").abs(),
            cli_version: "0.0.0".to_string(),
            source: codex_app_server_protocol::SessionSource::Cli,
            thread_source: Some(codex_app_server_protocol::ThreadSource::User),
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: Some("thread".to_string()),
            turns: vec![],
        },
        model: "gpt-5.4".to_string(),
        model_provider: "openai".to_string(),
        service_tier: None,
        cwd: test_path_buf("/tmp").abs(),
        runtime_workspace_roots: Vec::new(),
        instruction_sources: Vec::new(),
        approval_policy: codex_app_server_protocol::AskForApproval::OnRequest,
        sandbox: codex_app_server_protocol::SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        },
        permission_profile: None,
        active_permission_profile: None,
        reasoning_effort: None,
        selected_environment: None,
    }
}

#[test]
fn read_prompt_input_preserves_input_and_rejects_oversize_without_draining() {
    use std::io::Read as _;

    assert_eq!(
        super::read_prompt_input(b"normal prompt\n".as_slice()).unwrap(),
        b"normal prompt\n".to_vec()
    );
    let mut input = std::io::repeat(b'x').take(super::MAX_PROMPT_INPUT_BYTES + 2);
    let error = super::read_prompt_input(&mut input).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("exceeds"));
    assert_eq!(input.limit(), 1, "must stop after the first excess byte");
}
