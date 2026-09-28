use super::*;
use codex_login::CodexAuth;
use core_test_support::responses;
use std::time::Duration;

async fn lifecycle_case(new_window: bool) {
    let server = responses::start_mock_server().await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("AGENTS.md"),
        "Preserve WORLD_CONSTRAINT.\n",
    )
    .unwrap();
    let mut provider = codex_model_provider_info::built_in_model_providers(None)["openai"].clone();
    provider.name = "Context lifecycle test".into();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    provider.request_max_retries = Some(0);
    provider.stream_max_retries = Some(0);
    let (sess, turn, rx) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            CodexAuth::from_api_key("test-key"),
            vec![],
            home.path(),
            |config| {
                config.cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                    workspace.path(),
                )
                .unwrap();
                config.model_provider = provider;
                config.model_context_window = Some(258_000);
                config.model_auto_compact_token_limit = Some(240_000);
                for feature in [
                    Feature::CodeModeHost,
                    Feature::CodeMode,
                    Feature::Kd4Runtime,
                    Feature::EnableRequestCompression,
                ] {
                    config.features.disable(feature).unwrap();
                }
                config.features.enable(Feature::TokenBudget).unwrap();
                config.token_budget = Some(crate::config::TokenBudgetConfig::default());
            },
        )
        .await;
    let old_ids = sess.state.lock().await.auto_compact_window_ids();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(async move {
        started_tx.send(()).unwrap();
        std::future::pending().await
    });
    let aborted = pending.abort_handle();
    started_rx.await.unwrap();
    sess.set_session_startup_prewarm(
        crate::session_startup_prewarm::SessionStartupPrewarmHandle::new(
            pending,
            std::time::Instant::now(),
        ),
    )
    .await;
    let mut sequence = Vec::new();
    if new_window {
        sequence.push(responses::sse(vec![
            responses::ev_function_call("fresh-window", "new_context", "{}"),
            responses::ev_completed("switch-window"),
        ]));
    }
    sequence.push(responses::sse(vec![
        responses::ev_assistant_message("final", "LIFECYCLE_COMPLETE"),
        responses::ev_completed("final"),
    ]));
    let requests = responses::mount_sse_sequence(&server, sequence).await;
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        run_turn(
            Arc::clone(&sess),
            Arc::clone(&turn),
            Arc::new(codex_extension_api::ExtensionData::new("lifecycle")),
            vec![TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: "OLD_CONVERSATION_SENTINEL".into(),
                    text_elements: vec![],
                }],
                client_id: None,
            }],
            None,
            &mut LogicalGenerationBudget::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("unfinished prewarm must not block ordinary dispatch")
    .unwrap();
    assert_eq!(
        result.last_agent_message.as_deref(),
        Some("LIFECYCLE_COMPLETE")
    );
    while let Ok(event) = rx.try_recv() {
        assert!(!matches!(event.msg, EventMsg::Error(_)), "{event:?}");
    }
    assert!(aborted.is_finished(), "speculative task must be aborted");
    let captured = requests.requests();
    assert_eq!(captured.len(), if new_window { 2 } else { 1 });
    assert!(captured[0].body_contains_text("OLD_CONVERSATION_SENTINEL"));
    if new_window {
        let ids = sess.state.lock().await.auto_compact_window_ids();
        assert_ne!(ids.window_id, old_ids.window_id);
        assert_eq!(ids.first_window_id, old_ids.first_window_id);
        assert_eq!(ids.previous_window_id, Some(old_ids.window_id));
        assert!(!sess.new_context_window_requested().await);
        let next = &captured[1];
        assert!(!next.body_contains_text("OLD_CONVERSATION_SENTINEL"));
        assert!(next.body_contains_text("WORLD_CONSTRAINT"));
        assert!(next.body_contains_text(&workspace.path().display().to_string()));
        assert!(next.body_contains_text(&format!("Current context window id: {}", ids.window_id)));
        assert!(next.body_contains_text(&format!(
            "Previous context window id: {}",
            old_ids.window_id
        )));
    }
    sess.services.code_mode_service.shutdown().await.unwrap();
}

fn run_case(new_window: bool) {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(lifecycle_case(new_window));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn new_context_tool_installs_fresh_window_before_next_generation() {
    run_case(true);
}

#[test]
fn first_turn_dispatch_aborts_never_completing_prewarm() {
    run_case(false);
}

async fn validation_case(mutate_while_running: bool) {
    let server = responses::start_mock_server().await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    // Keep build products and synchronization writes outside the observed
    // source tree; only the mutation case changes validation inputs in flight.
    let artifacts = tempfile::tempdir().unwrap();
    let started = artifacts.path().join("started");
    let release = artifacts.path().join("release");
    let target = artifacts.path().join("target");
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"validation-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(root.join(".gitignore"), "/target\n/started\n/release\n").unwrap();
    let source = format!(
        r#"#[test]
fn product_path() {{
    std::fs::write({started:?}, "running").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !std::path::Path::new({release:?}).exists() {{
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }}
    assert!({mutate_while_running});
}}
"#
    );
    std::fs::write(root.join("src/lib.rs"), &source).unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }
    let built = std::process::Command::new("cargo")
        .args([
            "test",
            "--offline",
            "--quiet",
            "--no-run",
            "--target-dir",
        ])
        .arg(&target)
        .env_remove("CARGO_TARGET_DIR")
        .env("RUSTC_WRAPPER", "")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    if !mutate_while_running {
        std::fs::write(&release, "go").unwrap();
    }
    let mut provider = codex_model_provider_info::built_in_model_providers(None)["openai"].clone();
    provider.name = "Validation lifecycle test".into();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    provider.request_max_retries = Some(0);
    provider.stream_max_retries = Some(0);
    let (sess, mut turn, _rx) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            CodexAuth::from_api_key("test-key"),
            vec![],
            home.path(),
            |config| {
                config.cwd =
                    codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(root).unwrap();
                config.model_provider = provider;
                config.permissions.approval_policy = crate::config::Constrained::allow_any(
                    codex_protocol::protocol::AskForApproval::Never,
                );
                config
                    .permissions
                    .shell_environment_policy
                    .set
                    .insert("RUSTC_WRAPPER".into(), "".into());
                for feature in [
                    Feature::CodeModeHost,
                    Feature::CodeMode,
                    Feature::Kd4Runtime,
                    Feature::TokenBudget,
                    Feature::EnableRequestCompression,
                ] {
                    config.features.disable(feature).unwrap();
                }
                config.features.enable(Feature::ShellTool).unwrap();
                config.features.enable(Feature::UnifiedExec).unwrap();
            },
        )
        .await;
    Arc::get_mut(&mut turn).unwrap().permission_profile =
        codex_protocol::models::PermissionProfile::Disabled;
    let exec = |id| {
        responses::sse(vec![responses::ev_function_call(id, "exec_command", &serde_json::json!({
        "program":"cargo", "args":["test", "--offline", "--quiet", "--target-dir", target, "--", "--nocapture"], "yield_time_ms":10000, "max_output_tokens":1000,
    }).to_string()), responses::ev_completed(id)])
    };
    let mut sequence = vec![exec("validation-before")];
    if !mutate_while_running {
        sequence.push(responses::sse(vec![responses::ev_apply_patch_custom_tool_call("repair", "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-    assert!(false);\n+    assert!(true);\n*** End Patch"), responses::ev_completed("repair")]));
        sequence.push(exec("validation-after"));
    }
    sequence.push(responses::sse(vec![
        responses::ev_assistant_message("final", "VALIDATION_COMPLETE"),
        responses::ev_completed("final"),
    ]));
    let requests = responses::mount_response_sequence(&server, sequence.into_iter().enumerate().map(|(index, body)| {
        let response = wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream").set_body_string(body);
        // A zero-latency mock can dispatch the validation before the repair's
        // debounced OS events arrive. Model an ordinary response interval so
        // this asserts freshness after the repair, not watcher delivery timing.
        if !mutate_while_running && index == 2 {
            response.set_delay(Duration::from_millis(250))
        } else { response }
    }).collect()).await;
    let mutation = async {
        if mutate_while_running {
            tokio::time::timeout(Duration::from_secs(15), async {
                while !started.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("real validation process must start");
            std::fs::write(
                root.join("src/lib.rs"),
                format!("{source}\n// changed during validation\n"),
            )
            .unwrap();
            std::fs::write(&release, "go").unwrap();
        }
    };
    let run = async {
        run_turn(
            Arc::clone(&sess),
            Arc::clone(&turn),
            Arc::new(codex_extension_api::ExtensionData::new("validation")),
            vec![TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: "Run the test, repair its assertion if needed, and validate the repair."
                        .into(),
                    text_elements: vec![],
                }],
                client_id: None,
            }],
            None,
            &mut LogicalGenerationBudget::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(45), async {
        tokio::join!(run, mutation)
    })
    .await
    .unwrap();
    assert_eq!(
        result.last_agent_message.as_deref(),
        Some("VALIDATION_COMPLETE")
    );
    let captured = requests.requests();
    assert_eq!(captured.len(), if mutate_while_running { 2 } else { 4 });
    for request in &captured {
        let definitions = request.body_json()["tools"].to_string();
        assert!(!definitions.contains("workspace_validation"));
        assert!(!definitions.contains("semantic_context"));
    }
    let first = captured[1]
        .function_call_output_text("validation-before")
        .unwrap();
    assert!(
        first.contains(if mutate_while_running {
            "1 passed"
        } else {
            "1 failed"
        }),
        "{first}"
    );
    if mutate_while_running {
        assert!(
            validation_invalidation(&captured[1].body_json(), "validation-before").is_some(),
            "changed checkout must not inherit current validation proof"
        );
    } else {
        let last = captured[3]
            .function_call_output_text("validation-after")
            .unwrap();
        assert!(last.contains("1 passed"), "{last}");
        assert!(!last.contains("\"stale_workspace_evidence\":true"), "{last}");
        let invalidation = validation_invalidation(&captured[3].body_json(), "validation-after");
        assert!(invalidation.is_none(), "the repaired checkout must have a current pass: {invalidation:?}");
        assert!(
            std::fs::read_to_string(root.join("src/lib.rs"))
                .unwrap()
                .contains("assert!(true)")
        );
    }
    sess.services.code_mode_service.shutdown().await.unwrap();
}

fn validation_invalidation(request: &serde_json::Value, call_id: &str) -> Option<serde_json::Value> {
    request["input"].as_array().unwrap().iter().find_map(|item| {
        item["content"].as_array()?.iter().find_map(|part| {
            let text = part["text"].as_str()?;
            if !text.starts_with("<workspace_evidence_invalidation>\n") { return None; }
            let notice: serde_json::Value = serde_json::from_str(
                text.lines().find(|line| line.starts_with('{')).unwrap(),
            ).unwrap();
            (notice["call_id"] == call_id && notice["valid_for_current_workspace"] == false).then_some(notice)
        })
    })
}

#[test]
fn ordinary_validation_fails_repairs_and_passes_through_sampling() {
    run_validation_case(false);
}

#[test]
fn running_validation_source_mutation_cannot_be_current_proof() {
    run_validation_case(true);
}

fn run_validation_case(mutate: bool) {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(validation_case(mutate));
        })
        .unwrap()
        .join()
        .unwrap();
}
