use std::time::Duration;

use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use codex_app_server_protocol::EnvironmentInfoResponse;
use codex_app_server_protocol::EnvironmentShellInfo;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::time::timeout;

use super::exec_server_test_support::accept_exec_server_environment;

const RPC_TIMEOUT: Duration = Duration::from_secs(10);
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;
const INTERNAL_ERROR_CODE: i64 = -32603;



#[tokio::test]
async fn environment_info_rejects_unknown_environment() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(RPC_TIMEOUT, app_server.initialize()).await??;

    let request_id = app_server
        .send_raw_request(
            "environment/info",
            Some(json!({"environmentId": "missing"})),
        )
        .await?;
    let error = timeout(
        RPC_TIMEOUT,
        app_server.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        error,
        JSONRPCError {
            id: RequestId::Integer(request_id),
            error: JSONRPCErrorError {
                code: INVALID_REQUEST_ERROR_CODE,
                message: "unknown environment id `missing`".to_string(),
                data: None,
            },
        }
    );
    Ok(())
}


