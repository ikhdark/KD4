use std::net::SocketAddr;
use std::time::Duration;

use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCNotification;
use codex_exec_server_protocol::JSONRPCRequest;
use codex_exec_server_protocol::JSONRPCResponse;
use codex_exec_server_protocol::RequestId;
use pretty_assertions::assert_eq;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::duplex;
use tokio::time::timeout;

use super::DEFAULT_LISTEN_URL;
use super::ExecServerListenTransport;
use super::parse_listen_url;
use super::run_stdio_connection_with_io;
use crate::ExecServerRuntimePaths;
use crate::protocol::INITIALIZE_METHOD;
use crate::protocol::INITIALIZED_METHOD;
use crate::protocol::InitializeParams;
use crate::protocol::InitializeResponse;

#[test]
fn parse_listen_url_accepts_supported_transports() {
    for (url, expected) in [
        (DEFAULT_LISTEN_URL, ExecServerListenTransport::WebSocket("127.0.0.1:0".parse::<SocketAddr>().unwrap())),
        ("ws://127.0.0.1:1234", ExecServerListenTransport::WebSocket("127.0.0.1:1234".parse::<SocketAddr>().unwrap())),
        ("stdio", ExecServerListenTransport::Stdio),
        ("stdio://", ExecServerListenTransport::Stdio),
    ] {
        assert_eq!(parse_listen_url(url), Ok(expected), "{url}");
    }
}

#[tokio::test]
async fn stdio_listen_transport_serves_initialize() {
    let transport = parse_listen_url("stdio").expect("stdio listen URL should parse");
    let ExecServerListenTransport::Stdio = transport else {
        panic!("expected stdio listen transport, got {transport:?}");
    };

    let (mut client_writer, server_reader) = duplex(1 << 20);
    let (server_writer, client_reader) = duplex(1 << 20);
    let server_task = tokio::spawn(run_stdio_connection_with_io(
        server_reader,
        server_writer,
        test_runtime_paths(),
        crate::ExecServerTelemetry::default(),
    ));
    let mut client_lines = BufReader::new(client_reader).lines();

    let initialize = JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(1),
        method: INITIALIZE_METHOD.to_string(),
        params: Some(
            serde_json::to_value(InitializeParams {
                client_name: "exec-server-transport-test".to_string(),
                resume_session_id: None,
            })
            .expect("initialize params should serialize"),
        ),
        trace: None,
    });
    write_jsonrpc_line(&mut client_writer, &initialize).await;

    let response = timeout(Duration::from_secs(1), client_lines.next_line())
        .await
        .expect("initialize response should arrive")
        .expect("initialize response read should succeed")
        .expect("initialize response should be present");
    let response: JSONRPCMessage =
        serde_json::from_str(&response).expect("initialize response should parse");
    let JSONRPCMessage::Response(JSONRPCResponse { id, result }) = response else {
        panic!("expected initialize response, got {response:?}");
    };
    assert_eq!(id, RequestId::Integer(1));
    let initialize_response: InitializeResponse =
        serde_json::from_value(result).expect("initialize response should decode");
    uuid::Uuid::parse_str(&initialize_response.session_id)
        .expect("initialize should return a UUID session id");

    let initialized = JSONRPCMessage::Notification(JSONRPCNotification {
        method: INITIALIZED_METHOD.to_string(),
        params: Some(serde_json::to_value(()).expect("initialized params should serialize")),
    });
    write_jsonrpc_line(&mut client_writer, &initialized).await;

    drop(client_writer);
    drop(client_lines);
    timeout(Duration::from_secs(1), server_task)
        .await
        .expect("stdio transport should finish after client disconnect")
        .expect("stdio transport task should join")
        .expect("stdio transport should not fail");
}

#[test]
fn parse_listen_url_rejects_invalid_and_unsupported_urls() {
    for (url, message) in [
        ("ws://localhost:1234", "invalid websocket --listen URL `ws://localhost:1234`; expected `ws://IP:PORT`"),
        ("http://127.0.0.1:1234", "unsupported --listen URL `http://127.0.0.1:1234`; expected `ws://IP:PORT` or `stdio`"),
    ] {
        assert_eq!(parse_listen_url(url).unwrap_err().to_string(), message);
    }
}

async fn write_jsonrpc_line(writer: &mut tokio::io::DuplexStream, message: &JSONRPCMessage) {
    let encoded = serde_json::to_vec(message).expect("JSON-RPC message should serialize");
    writer
        .write_all(&encoded)
        .await
        .expect("JSON-RPC message should write");
    writer
        .write_all(b"\n")
        .await
        .expect("JSON-RPC newline should write");
}

fn test_runtime_paths() -> ExecServerRuntimePaths {
    ExecServerRuntimePaths::new(std::env::current_exe().expect("current exe"))
        .expect("runtime paths")
}

#[tokio::test]
async fn plain_websocket_rejects_non_loopback_before_listening() {
    for address in ["0.0.0.0:0", "[::]:0", "192.0.2.1:0"] {
        let error = super::run_websocket_listener(
            address.parse().unwrap(),
            test_runtime_paths(),
            crate::ExecServerTelemetry::default(),
        )
        .await
        .expect_err("unauthenticated remote listener must fail closed");
        assert!(
            error.to_string().contains("requires a loopback address"),
            "{error}"
        );
    }
}
