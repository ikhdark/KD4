use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::create_fake_rollout;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadDeleteParams;
use codex_app_server_protocol::ThreadDeleteResponse;
use codex_app_server_protocol::ThreadDeletedNotification;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_core::find_thread_path_by_id_str;
use codex_protocol::ThreadId;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn thread_delete_deletes_spawned_descendants() -> Result<()> {
    let codex_home = TempDir::new()?;

    let parent_id = create_delete_test_rollout(codex_home.path(), /*minute*/ 0, "parent")?;
    let child_id = create_delete_test_rollout(codex_home.path(), /*minute*/ 1, "child")?;
    let grandchild_id =
        create_delete_test_rollout(codex_home.path(), /*minute*/ 2, "grandchild")?;

    let state_db =
        StateRuntime::init(codex_home.path().to_path_buf(), "mock_provider".into()).await?;
    let parent_thread_id = ThreadId::from_string(&parent_id)?;
    let child_thread_id = ThreadId::from_string(&child_id)?;
    let grandchild_thread_id = ThreadId::from_string(&grandchild_id)?;

    for (parent, child, status) in [
        (
            parent_thread_id,
            child_thread_id,
            DirectionalThreadSpawnEdgeStatus::Closed,
        ),
        (
            child_thread_id,
            grandchild_thread_id,
            DirectionalThreadSpawnEdgeStatus::Open,
        ),
    ] {
        state_db
            .upsert_thread_spawn_edge(parent, child, status)
            .await?;
    }

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let delete_id = mcp
        .send_thread_delete_request(ThreadDeleteParams {
            thread_id: parent_id.clone(),
        })
        .await?;
    let delete_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(delete_id)),
    )
    .await??;
    let _: ThreadDeleteResponse = to_response::<ThreadDeleteResponse>(delete_resp)?;

    let mut deleted_ids = Vec::new();
    for _ in 0..3 {
        let notification = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_notification_message("thread/deleted"),
        )
        .await??;
        let deleted_notification: ThreadDeletedNotification = serde_json::from_value(
            notification
                .params
                .expect("thread/deleted notification params"),
        )?;
        deleted_ids.push(deleted_notification.thread_id);
    }
    assert_eq!(deleted_ids, vec![grandchild_id, child_id, parent_id]);

    for thread_id in [parent_thread_id, child_thread_id, grandchild_thread_id] {
        let rollout_path = find_thread_path_by_id_str(
            codex_home.path(),
            &thread_id.to_string(),
            /*state_db_ctx*/ None,
        )
        .await?;
        assert!(
            rollout_path.is_none(),
            "expected active rollout for {thread_id} to be deleted"
        );
    }
    assert_eq!(
        state_db
            .list_thread_spawn_descendants(parent_thread_id)
            .await?,
        Vec::<ThreadId>::new()
    );
    Ok(())
}

fn create_delete_test_rollout(codex_home: &Path, minute: u8, preview: &str) -> Result<String> {
    create_fake_rollout(
        codex_home,
        &format!("2025-01-01T00-{minute:02}-00"),
        &format!("2025-01-01T00:{minute:02}:00Z"),
        preview,
        Some("mock_provider"),
        /*git_info*/ None,
    )
}

#[tokio::test]
async fn thread_delete_handles_live_threads_before_rollout_exists() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let persisted_thread = to_response::<ThreadStartResponse>(start_resp)?.thread;
    let rollout_path = find_thread_path_by_id_str(
        codex_home.path(),
        &persisted_thread.id,
        /*state_db_ctx*/ None,
    )
    .await?;
    assert_eq!(rollout_path, None);

    let delete_id = mcp
        .send_thread_delete_request(ThreadDeleteParams {
            thread_id: persisted_thread.id,
        })
        .await?;
    let delete_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(delete_id)),
    )
    .await??;
    let _: ThreadDeleteResponse = to_response::<ThreadDeleteResponse>(delete_resp)?;

    let start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            ephemeral: Some(true),
            ..Default::default()
        })
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(start_resp)?;

    let delete_id = mcp
        .send_thread_delete_request(ThreadDeleteParams {
            thread_id: thread.id.clone(),
        })
        .await?;
    let delete_err: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(delete_id)),
    )
    .await??;
    let expected_message = format!(
        "thread is not persisted and cannot be deleted: {}",
        thread.id
    );
    assert_eq!(delete_err.error.message, expected_message);

    let list_id = mcp
        .send_thread_loaded_list_request(ThreadLoadedListParams::default())
        .await?;
    let list_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(list_id)),
    )
    .await??;
    let ThreadLoadedListResponse { mut data, .. } =
        to_response::<ThreadLoadedListResponse>(list_resp)?;
    data.sort();
    assert_eq!(data, vec![thread.id]);

    Ok(())
}

#[tokio::test]
async fn thread_delete_failure_preserves_loaded_thread() -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let start_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let thread = to_response::<ThreadStartResponse>(start_response)?.thread;

    send_turn_and_wait(&mut mcp, &thread.id, "materialize").await?;
    let rollout_path = thread.path.expect("thread path");
    assert!(rollout_path.exists());

    let _rollout_lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x0000_0001 | 0x0000_0002)
        .open(&rollout_path)?;
    let delete_id = mcp
        .send_thread_delete_request(ThreadDeleteParams {
            thread_id: thread.id.clone(),
        })
        .await?;
    let delete_error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(delete_id)),
    )
    .await??;
    assert!(delete_error.error.message.contains("cannot be deleted"));
    assert!(rollout_path.exists());

    send_turn_and_wait(&mut mcp, &thread.id, "still loaded").await?;

    Ok(())
}

async fn send_turn_and_wait(mcp: &mut TestAppServer, thread_id: &str, text: &str) -> Result<()> {
    let turn_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.to_string(),
            client_user_message_id: None,
            input: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_id)),
    )
    .await??;
    let _: TurnStartResponse = to_response(response)?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    Ok(())
}

fn create_config_toml(codex_home: &Path, server_uri: &str) -> std::io::Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}
