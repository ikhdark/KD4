//! Production-path regressions derived from the handler efficiency review probes.

use super::*;
use crate::environment_selection::ThreadEnvironments;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::step_context::StepContext;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::handlers::ReadFileHandler;
use crate::tools::handlers::WaitForEnvironmentHandler;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolRouter;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

#[expect(clippy::print_stdout, reason = "emits benchmark measurements")]
fn report(value: serde_json::Value) {
    if let Some(directory) = std::env::var_os("CODEX_TOOL_HISTORY_BENCH_OUTPUT_DIR") {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("{}.json", value["case"].as_str().unwrap())),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
    }
    println!("HANDLER_EFFICIENCY_BENCH {value}");
}

fn workspace() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.path().join("source.txt"), "original source\n").unwrap();
    root
}

fn call(name: &str, id: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        tool_name: codex_tools::ToolName::plain(name),
        call_id: id.into(),
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    }
}

fn function_value(response: &ResponseInputItem) -> serde_json::Value {
    let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
        panic!("expected function output");
    };
    serde_json::from_str(output.text_content().unwrap()).unwrap()
}

fn invalidated_calls(items: &[ResponseItem]) -> BTreeSet<String> {
    items
        .iter()
        .filter_map(|item| {
            let ResponseItem::Message { role, content, .. } = item else {
                return None;
            };
            if role != "developer" {
                return None;
            }
            content.iter().find_map(|part| {
                let codex_protocol::models::ContentItem::InputText { text } = part else {
                    return None;
                };
                let body = text.lines().find(|line| line.starts_with('{'))?;
                let value: serde_json::Value = serde_json::from_str(body).ok()?;
                (value["stale_workspace_evidence"] == true)
                    .then(|| value["call_id"].as_str().unwrap().to_string())
            })
        })
        .collect()
}

#[tokio::test]
async fn notice_deduplication_before_after() {
    const READS: usize = 16;
    let root = workspace();
    std::fs::write(root.path().join(".gitignore"), "source.txt\n").unwrap();
    let cache = GitWorkspaceCache::new();
    let proof = cache
        .begin_source_path_change_observation(root.path(), &root.path().join("source.txt"), false)
        .await
        .unwrap();
    let revision = cache
        .workspace_evidence_identity(root.path())
        .await
        .unwrap();
    assert!(!revision.unavailable);
    let mut state = ToolHistoryState::default();
    let mut canonical = Vec::new();
    for index in 0..READS {
        let id = format!("notice-{index}");
        let output = text_output(&id, "source evidence\n".repeat(128));
        state.register_workspace_evidence(
            WorkspaceEvidenceObservation::from_response_item(
                Some(revision.clone()),
                &output,
                BTreeSet::from([SourceDependencyV1::new(
                    &root.path().join("source.txt"),
                    false,
                )]),
            )
            .unwrap()
            .with_source_path_observations(vec![proof.clone()]),
        );
        canonical.extend([function_call(&id), output]);
    }
    let canonical: Arc<[ResponseItem]> = canonical.into();
    let mut anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&canonical),
        projection: state.project_sampling_with_workspace_cache(
            Arc::clone(&canonical),
            Some(&revision),
            &cache,
        ),
    };
    assert_eq!(anchor.projection.items, canonical);
    std::fs::write(root.path().join("source.txt"), "modified source\n").unwrap();
    timeout(Duration::from_secs(10), async {
        while cache.source_path_change_observation_is_current(&proof) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        cache
            .workspace_evidence_identity(root.path())
            .await
            .unwrap(),
        revision
    );
    let mut baseline = canonical.to_vec();
    for index in 0..=8 {
        if index > 0 {
            std::fs::write(
                root.path().join("other.txt"),
                format!("unrelated {index}\n"),
            )
            .unwrap();
            cache
                .note_host_workspace_mutation_paths(root.path(), &["other.txt".into()])
                .await;
        }
        let current = cache
            .workspace_evidence_identity(root.path())
            .await
            .unwrap();
        if index > 0 {
            state.invalidate_source_dependencies(
                Some(&BTreeSet::from([root.path().join("other.txt")])),
                Some(&current),
            );
        }
        // Generate notices using the real freshness code, then replay the old
        // append policy: full ResponseItem equality, not the new semantic key.
        let fresh = state.project_sampling_with_workspace_cache(
            Arc::clone(&canonical),
            Some(&current),
            &cache,
        );
        assert_eq!(invalidated_calls(&fresh.items).len(), READS);
        for notice in &fresh.items[canonical.len()..] {
            if !baseline.contains(notice) {
                baseline.push(notice.clone());
            }
        }
        let next = state
            .project_continuation_with_workspace_cache(
                &anchor,
                Arc::clone(&canonical),
                Some(&current),
                &cache,
            )
            .unwrap();
        assert!(next.items.starts_with(&anchor.projection.items));
        anchor.projection = next;
    }
    let before = &baseline[canonical.len()..];
    let after = &anchor.projection.items[canonical.len()..];
    assert_eq!(before.len(), READS * 9);
    assert_eq!(after.len(), READS);
    assert_eq!(
        invalidated_calls(&baseline),
        invalidated_calls(&anchor.projection.items)
    );
    let mut notice = json!({"call_id":"guard", "stale_workspace_evidence":true,
        "valid_for_current_workspace":false,"reason_code":"workspace_freshness_unverified"});
    let original_key = workspace_invalidation_key(&notice).unwrap();
    notice["reason_code"] = json!("output_mismatch");
    assert_ne!(workspace_invalidation_key(&notice).unwrap(), original_key);
    notice["reason_code"] = json!("workspace_freshness_unverified");
    notice["current_nested_results"] = json!([{"call_id":"still-current"}]);
    assert_ne!(workspace_invalidation_key(&notice).unwrap(), original_key);
    let tokens = |items: &[ResponseItem]| {
        items
            .iter()
            .map(|item| {
                let ResponseItem::Message { content, .. } = item else {
                    panic!("notice")
                };
                content
                    .iter()
                    .map(|part| match part {
                        codex_protocol::models::ContentItem::InputText { text } => {
                            approx_token_count(text)
                        }
                        _ => panic!("text notice"),
                    })
                    .sum::<usize>()
            })
            .sum::<usize>()
    };
    report(
        json!({"case":"notice_deduplication", "observations":READS,"unrelated_edits":8,
        "baseline_notices":before.len(),"current_notices":after.len(),
        "baseline_approx_tokens":tokens(before),"current_approx_tokens":tokens(after),
        "measurement_scope":"old full-message equality replay vs current production deduplication; actual watcher and projection; no model timing"}),
    );
}

#[tokio::test]
async fn explicit_local_read_dependencies() {
    const READS: usize = 2;
    let root = workspace();
    let fallback = workspace();
    std::fs::write(fallback.path().join("source.txt"), "wrong environment\n").unwrap();
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config).cwd =
        AbsolutePathBuf::from_absolute_path(fallback.path()).unwrap();
    turn.permission_profile = PermissionProfile::Disabled;
    turn.environments.turn_environments = vec![crate::session::turn_context::TurnEnvironment::new(
        "local".into(),
        Arc::new(codex_exec_server::Environment::default_for_tests()),
        PathUri::from_host_native_path(root.path()).unwrap(),
        None,
    )];
    let session = Arc::new(session);
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([Arc::new(ReadFileHandler) as Arc<dyn CoreToolRuntime>]),
        Vec::new(),
    ));
    let runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        StepContext::for_test(Arc::new(turn)).with_tool_router_for_test(router),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let mut canonical = Vec::new();
    for index in 0..READS {
        for explicit in [true, false] {
            let id = format!("{}-{index}", if explicit { "explicit" } else { "default" });
            let mut args = json!({"path":"source.txt"});
            if explicit {
                args["environment_id"] = json!("local");
            }
            let invocation = call("read_file", &id, args.clone());
            let response = if index == 0 {
                let result = runtime
                    .clone()
                    .handle_tool_call_with_source(
                        invocation,
                        ToolCallSource::CodeMode {
                            cell_id: "benchmark-cell".into(),
                            parent_call_id: Some("benchmark-parent".into()),
                            runtime_tool_call_id: id.clone(),
                            nested_deadline: None,
                            cancellation_cause: None,
                        },
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    result.projected_source_dependencies().unwrap(),
                    &BTreeSet::from([SourceDependencyV1::new(
                        &root.path().join("source.txt"),
                        false
                    )])
                );
                result.response()
            } else {
                runtime
                    .clone()
                    .handle_tool_call(invocation, CancellationToken::new())
                    .await
                    .unwrap()
            };
            let value = function_value(&response);
            canonical.push(named_function_call_with_arguments(&id, "read_file", args));
            canonical.push(ResponseItem::from(response));
            assert_eq!(value["results"][0]["text"], "original source\n");
            assert_eq!(value["file_complete"], true);
        }
    }
    let canonical: Arc<[ResponseItem]> = canonical.into();
    let mut state = session.clone_history().await.tool_history_state();
    let cache = &session.services.git_workspace;
    std::fs::write(root.path().join("other.txt"), "unrelated change\n").unwrap();
    cache
        .note_host_workspace_mutation_paths(root.path(), &["other.txt".into()])
        .await;
    let revision = cache
        .workspace_evidence_identity(root.path())
        .await
        .unwrap();
    state.invalidate_source_dependencies(
        Some(&BTreeSet::from([root.path().join("other.txt")])),
        Some(&revision),
    );
    let projection =
        state.project_sampling_with_workspace_cache(Arc::clone(&canonical), Some(&revision), cache);
    let false_stale = invalidated_calls(&projection.items);
    assert!(false_stale.is_empty());
    assert_eq!(projection.items, canonical);
    std::fs::write(root.path().join("source.txt"), "modified source\n").unwrap();
    cache
        .note_host_workspace_mutation_paths(root.path(), &["source.txt".into()])
        .await;
    let revision = cache
        .workspace_evidence_identity(root.path())
        .await
        .unwrap();
    state.invalidate_source_dependencies(
        Some(&BTreeSet::from([root.path().join("source.txt")])),
        Some(&revision),
    );
    let changed = state.project_sampling_with_workspace_cache(canonical, Some(&revision), cache);
    assert_eq!(invalidated_calls(&changed.items).len(), READS * 2);
    let fresh = runtime
        .handle_tool_call(
            call(
                "read_file",
                "fresh",
                json!({"path":"source.txt", "environment_id":"local"}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        function_value(&fresh)["results"][0]["text"],
        "modified source\n"
    );
}

#[tokio::test]
async fn file_classification_uses_only_ready_native_local_environments() {
    use crate::session::turn_context::TurnEnvironment;

    let root = tempfile::tempdir().unwrap();
    let selected = root.path().join("selected");
    std::fs::create_dir(&selected).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let manager = EnvironmentManager::create_for_tests_with_local(
        Some(format!("ws://{}", listener.local_addr().unwrap())),
        ExecServerRuntimePaths::new(std::env::current_exe().unwrap()).unwrap(),
    )
    .await;
    let local = manager.get_environment("local").unwrap();
    let remote = manager.get_environment("remote").unwrap();
    assert!(!local.is_remote());
    assert!(remote.is_remote());
    let local_cwd = PathUri::from_host_native_path(&selected).unwrap();
    let mut environments = TurnEnvironmentSnapshot {
        turn_environments: vec![
        TurnEnvironment::new("remote".into(), remote, local_cwd.clone(), None),
        TurnEnvironment::new("local".into(), Arc::clone(&local), local_cwd, None),
        ],
        ..Default::default()
    };
    for tool in ["read_file", "list_files"] {
        let args = json!({"path":"owner", "environment_id":"local"});
        let payload = ToolPayload::Function {
            arguments: args.to_string(),
        };
        let classification = classify_workspace_tool_call_in_environments(
            tool,
            &payload,
            root.path(),
            Some(&environments),
        );
        assert_eq!(classification.workspace_cwd, selected);
        assert_eq!(
            classification.source_dependencies,
            BTreeSet::from([SourceDependencyV1::new(
                &selected.join("owner"),
                tool == "list_files"
            ),])
        );
        for parsed in [None, Some(&args)] {
            assert_eq!(
                source_dependencies_for_tool_call_with_parsed_arguments(
                    tool,
                    &payload,
                    parsed,
                    root.path(),
                    Some(&environments),
                ),
                classification.source_dependencies
            );
        }
        for args in [
            json!({"path":"owner"}),
            json!({"path":"owner", "environment_id":"remote"}),
            json!({"path":"owner", "environment_id":"starting-or-missing"}),
            json!({"path":"owner", "environment_id":42}),
        ] {
            let unknown = classify_workspace_tool_call_in_environments(
                tool,
                &ToolPayload::Function {
                    arguments: args.to_string(),
                },
                root.path(),
                Some(&environments),
            );
            assert!(unknown.observes_workspace);
            assert!(unknown.source_dependencies.is_empty());
        }
    }
    environments.turn_environments.swap(0, 1);
    let payload = ToolPayload::Function {
        arguments: json!({"path":"owner"}).to_string(),
    };
    assert_eq!(
        classify_workspace_tool_call_in_environments(
            "read_file",
            &payload,
            root.path(),
            Some(&environments),
        )
        .source_dependencies,
        BTreeSet::from([SourceDependencyV1::new(&selected.join("owner"), false)])
    );
    let foreign = PathUri::parse(if cfg!(windows) {
        "file:///foreign/root"
    } else {
        "file:///Z:/foreign"
    })
    .unwrap();
    environments.turn_environments =
        vec![TurnEnvironment::new("local".into(), local, foreign, None)];
    assert!(
        classify_workspace_tool_call_in_environments(
            "read_file",
            &payload,
            root.path(),
            Some(&environments),
        )
        .source_dependencies
        .is_empty()
    );
    environments.turn_environments.clear();
    assert!(
        classify_workspace_tool_call_in_environments(
            "read_file",
            &payload,
            root.path(),
            Some(&environments),
        )
        .source_dependencies
        .is_empty()
    );
    let skill = classify_workspace_tool_call_in_environments(
        "read_file",
        &ToolPayload::Function {
            arguments: json!({"path":"skill:example"}).to_string(),
        },
        root.path(),
        Some(&environments),
    );
    assert!(!skill.observes_workspace);
    assert!(skill.source_dependencies.is_empty());
}

struct WaitVariant {
    entered: Arc<Notify>,
}

impl ToolExecutor<ToolInvocation> for WaitVariant {
    fn tool_name(&self) -> codex_tools::ToolName {
        WaitForEnvironmentHandler.tool_name()
    }
    fn spec(&self) -> codex_tools::ToolSpec {
        WaitForEnvironmentHandler.spec()
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        WaitForEnvironmentHandler.supports_parallel_tool_calls()
    }
    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            self.entered.notify_one();
            WaitForEnvironmentHandler.handle(invocation).await
        })
    }
}
impl CoreToolRuntime for WaitVariant {}

async fn next_json(socket: &mut WebSocketStream<TcpStream>) -> serde_json::Value {
    loop {
        match timeout(Duration::from_secs(10), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Binary(bytes) => return serde_json::from_slice(&bytes).unwrap(),
            Message::Ping(_) | Message::Pong(_) => {}
            frame => panic!("unexpected frame {frame:?}"),
        }
    }
}

async fn handshake(listener: TcpListener, release: oneshot::Receiver<bool>, cwd: PathUri) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = accept_async(stream).await.unwrap();
    let init = next_json(&mut socket).await;
    assert_eq!(init["method"], "initialize");
    socket
        .send(Message::Text(
            json!({"id":init["id"],"result":{"sessionId":"handler-benchmark"}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut socket).await["method"], "initialized");
    let info = next_json(&mut socket).await;
    assert_eq!(info["method"], "environment/info");
    if release.await.unwrap_or(false) {
        let shell = crate::shell::default_user_shell();
        socket
            .send(Message::Text(
                json!({"id":info["id"],"result":{
                    "operatingSystem":std::env::consts::OS,"cwd":cwd.to_string(),
                    "shell":{"name":shell.name(),"path":shell.shell_path.to_string_lossy()}
                }})
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
    }
}

async fn wait_sample(outcome: &str) {
    let root = workspace();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let manager = Arc::new(
        EnvironmentManager::create_for_tests_with_local(
            Some(format!("ws://{}", listener.local_addr().unwrap())),
            ExecServerRuntimePaths::new(std::env::current_exe().unwrap()).unwrap(),
        )
        .await,
    );
    let cwd = PathUri::from_host_native_path(root.path()).unwrap();
    let environments = ThreadEnvironments::new(
        manager,
        crate::shell::default_user_shell(),
        crate::shell_snapshot::ShellSnapshot::disabled(),
        TurnEnvironmentSnapshot::default(),
        true,
    );
    environments.update_selections(&[
        TurnEnvironmentSelection {
            environment_id: "local".into(),
            cwd: cwd.clone(),
        },
        TurnEnvironmentSelection {
            environment_id: "remote".into(),
            cwd: cwd.clone(),
        },
    ]);
    let snapshot = environments.snapshot().await;
    assert_eq!(snapshot.starting.len(), 1);
    let (release, released) = oneshot::channel();
    let server = tokio::spawn(handshake(listener, released, cwd));
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config).cwd = AbsolutePathBuf::from_absolute_path(root.path()).unwrap();
    turn.permission_profile = PermissionProfile::Disabled;
    turn.environments = snapshot.clone();
    let entered = Arc::new(Notify::new());
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([
            Arc::new(WaitVariant {
                entered: Arc::clone(&entered),
            }) as Arc<dyn CoreToolRuntime>,
            Arc::new(ReadFileHandler) as Arc<dyn CoreToolRuntime>,
        ]),
        Vec::new(),
    ));
    let mut step = StepContext::for_test(Arc::new(turn));
    Arc::make_mut(&mut step).environments = snapshot;
    let runtime = ToolCallRuntime::new(
        Arc::new(session),
        step.with_tool_router_for_test(router),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let warm = runtime
        .clone()
        .handle_tool_call(
            call("read_file", "warm", json!({"path":"source.txt"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        function_value(&warm)["results"][0]["text"],
        "original source\n"
    );
    let cancel = CancellationToken::new();
    let wait = tokio::spawn(runtime.clone().handle_tool_call(
        call(
            "wait_for_environment",
            "wait",
            json!({"environment_id":"remote"}),
        ),
        cancel.clone(),
    ));
    timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    let read = tokio::spawn(async move {
        runtime
            .handle_tool_call(
                call("read_file", "sibling", json!({"path":"source.txt"})),
                CancellationToken::new(),
            )
            .await
            .unwrap()
    });
    let read_response = timeout(Duration::from_secs(2), read)
        .await
        .expect("independent read must complete while the handshake is withheld")
        .unwrap();
    assert!(
        !wait.is_finished(),
        "readiness must not be reported before the handshake"
    );
    let wait_result = if outcome == "cancel" {
        cancel.cancel();
        let response = timeout(Duration::from_secs(2), wait)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        release.send(false).unwrap();
        let ResponseInputItem::FunctionCallOutput { output, .. } = &response else {
            panic!("function output")
        };
        assert!(output.text_content().unwrap().contains("aborted by user"));
        response
    } else {
        release.send(outcome == "ready").unwrap();
        timeout(Duration::from_secs(10), wait)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    };
    server.await.unwrap();
    assert_eq!(
        function_value(&read_response)["results"][0]["text"],
        "original source\n"
    );
    if outcome == "ready" {
        assert_eq!(
            function_value(&wait_result),
            json!({"environment_id":"remote","status":"ready"})
        );
    } else if outcome == "failure" {
        let ResponseInputItem::FunctionCallOutput { output, .. } = &wait_result else {
            panic!("function output")
        };
        assert_eq!(output.success, Some(false));
        assert!(output.text_content().unwrap().contains("failed to start"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_wait_concurrency() {
    for outcome in ["ready", "failure", "cancel"] {
        wait_sample(outcome).await;
    }
}
