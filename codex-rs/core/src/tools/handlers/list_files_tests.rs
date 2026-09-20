use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ToolCallSource;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::models::PermissionProfile;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

async fn invocation(root: &std::path::Path, arguments: serde_json::Value) -> ToolInvocation {
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = PermissionProfile::Disabled;
    turn.environments.turn_environments = vec![TurnEnvironment::new(
        codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        Arc::new(codex_exec_server::Environment::default_for_tests()),
        PathUri::from_host_native_path(root).unwrap(),
        None,
    )];
    ToolInvocation {
        session: Arc::new(session),
        step_context: StepContext::for_test(Arc::new(turn)),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "list-files".into(),
        tool_name: ToolName::plain("list_files"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    }
}

#[tokio::test]
async fn registered_listing_tracks_directory_evidence_and_bounds_traversal() {
    use crate::tool_history::SourceDependencyV1;
    use crate::tools::parallel::ToolCallRuntime;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::router::ToolCall;
    use crate::tools::router::ToolRouter;
    use std::collections::BTreeSet;

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::create_dir(root.path().join(".hidden")).unwrap();
    std::fs::write(root.path().join("src/file.txt"), "content").unwrap();
    std::fs::write(root.path().join(".hidden/secret.txt"), "hidden").unwrap();
    for (extra, expected_count, truncated) in [
        (json!({}), 3, false),
        (json!({"include_hidden": true}), 4, false),
        (json!({"max_entries": 1}), 1, true),
        (json!({"max_depth": 0}), 2, true),
    ] {
        let mut arguments = json!({"path": root.path()});
        arguments
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let call = invocation(root.path(), arguments).await;
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([Arc::new(ListFilesHandler) as Arc<dyn CoreToolRuntime>]),
            Vec::new(),
        ));
        let runtime = ToolCallRuntime::new(
            call.session,
            call.step_context.with_tool_router_for_test(router),
            call.tracker,
        );
        let result = runtime
            .handle_tool_call_with_source(
                ToolCall {
                    tool_name: call.tool_name,
                    call_id: call.call_id,
                    payload: call.payload,
                },
                ToolCallSource::CodeMode {
                    cell_id: "list-cell".into(),
                    parent_call_id: Some("outer".into()),
                    runtime_tool_call_id: "nested-list".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                },
                call.cancellation_token,
            )
            .await
            .unwrap();
        let dependencies = result.projected_source_dependencies().cloned();
        let output = result.code_mode_result();
        assert_eq!(output["entries"].as_array().unwrap().len(), expected_count);
        assert_eq!(output["truncated"], truncated);
        assert_eq!(output["complete"], !truncated);
        assert_eq!(output["errors"], json!([]));
        assert_eq!(
            dependencies.as_ref(),
            Some(&BTreeSet::from([SourceDependencyV1::new(
                root.path(),
                true
            ),]))
        );
        if expected_count == 3 {
            assert!(output.to_string().contains("file.txt"));
            assert!(!output.to_string().contains("secret.txt"));
        }
    }
}

#[tokio::test]
async fn listing_rejects_invalid_limits_missing_roots_and_cancelled_calls() {
    let root = tempfile::tempdir().unwrap();
    let ToolSpec::Function(spec) = ListFilesHandler.spec() else {
        panic!("function spec")
    };
    let schema = serde_json::to_value(spec.parameters).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for arguments in [
        json!({"path": ".", "max_entries": 0}),
        json!({"path": ".", "max_entries": 50_001}),
        json!({"path": ".", "max_depth": 65}),
        json!({"path": ".", "max_depth": -1}),
        json!({"path": ".", "typo": true}),
    ] {
        assert!(!validator.is_valid(&arguments));
        assert!(
            ListFilesHandler
                .handle(invocation(root.path(), arguments).await)
                .await
                .is_err()
        );
    }
    assert!(
        ListFilesHandler
            .handle(invocation(root.path(), json!({"path": "missing"})).await)
            .await
            .is_err()
    );
    let cancelled = invocation(root.path(), json!({"path": "."})).await;
    cancelled.cancellation_token.cancel();
    let error = ListFilesHandler.handle(cancelled).await.err().unwrap();
    assert!(error.to_string().contains("cancelled"));
}

#[tokio::test]
async fn listing_does_not_follow_directory_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("outside.txt"), "outside").unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(outside.path(), root.path().join("link")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
    let call = invocation(root.path(), json!({"path": "."})).await;
    let payload = call.payload.clone();
    let output = ListFilesHandler
        .handle(call)
        .await
        .unwrap()
        .code_mode_result(&payload);
    assert_eq!(output["complete"], true);
    assert!(!output.to_string().contains("outside.txt"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_listing_forwards_sandbox_and_preserves_partial_errors() {
    use futures::SinkExt;
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let root = tempfile::tempdir().unwrap();
    let cwd = PathUri::from_host_native_path(root.path()).unwrap();
    let remote_file = cwd.join("remote-only.txt").unwrap();
    let response_file = remote_file.clone();
    let response_cwd = cwd.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut walks = Vec::new();
        loop {
            let frame =
                tokio::select! { frame = socket.next() => frame, _ = &mut stop_rx => break };
            let Some(Ok(frame)) = frame else { break };
            let message: serde_json::Value = match frame {
                Message::Text(text) => serde_json::from_str(text.as_ref()).unwrap(),
                Message::Binary(bytes) => serde_json::from_slice(bytes.as_ref()).unwrap(),
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Close(_) => break,
                other => panic!("unexpected frame {other:?}"),
            };
            let result = match message["method"].as_str().unwrap() {
                "initialize" => json!({"sessionId": "remote-listing"}),
                "initialized" => continue,
                "environment/info" => {
                    json!({"operatingSystem": "windows", "shell": {"name": "cmd", "path": "cmd.exe"}, "cwd": response_cwd})
                }
                "fs/walk" => {
                    walks.push(message["params"].clone());
                    json!({"entries": [{"path": response_file, "kind": "file"}],
                           "errors": [{"path": response_cwd, "message": "permission denied for a descendant"}], "truncated": true})
                }
                method => panic!("unexpected filesystem operation {method}"),
            };
            socket
                .send(Message::Text(
                    json!({"id": message["id"], "result": result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        }
        walks
    });
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = PermissionProfile::read_only();
    turn.environments.turn_environments = vec![TurnEnvironment::new(
        "remote-listing".into(),
        Arc::new(codex_exec_server::Environment::create_for_tests(Some(url)).unwrap()),
        cwd.clone(),
        None,
    )];
    let mut expected_sandbox = turn.file_system_sandbox_context(None, &cwd);
    expected_sandbox.cwd = None;
    expected_sandbox.workspace_roots.clear();
    let payload = ToolPayload::Function {
        arguments: json!({"path": ".", "environment_id": "remote-listing", "max_entries": 17})
            .to_string(),
    };
    let call = ToolInvocation {
        session: Arc::new(session),
        step_context: StepContext::for_test(Arc::new(turn)),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "remote-list".into(),
        tool_name: ToolName::plain("list_files"),
        source: ToolCallSource::Direct,
        payload: payload.clone(),
    };
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        ListFilesHandler.handle(call),
    )
    .await
    .unwrap()
    .unwrap()
    .code_mode_result(&payload);
    stop_tx.send(()).unwrap();
    let walks = server.await.unwrap();
    assert_eq!(walks.len(), 1);
    assert_eq!(walks[0]["sandbox"], json!(expected_sandbox));
    assert_eq!(walks[0]["options"]["maxEntries"], 17);
    assert_eq!(walks[0]["options"]["followDirectorySymlinks"], false);
    assert_eq!(output["entries"][0]["path"], json!(remote_file));
    assert_eq!(output["complete"], false);
    assert_eq!(output["truncated"], true);
    assert_eq!(
        output["errors"][0]["message"],
        "permission denied for a descendant"
    );
    assert!(!root.path().join("remote-only.txt").exists());
}
