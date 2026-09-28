use std::sync::Arc;
use std::time::Duration;

use codex_api::AuthProvider;
use codex_api::Provider;
use codex_api::ResponseCreateWsRequest;
use codex_api::ResponseEvent;
use codex_api::ResponsesWebsocketClient;
use codex_api::ResponsesWsRequest;
use codex_api::RetryConfig;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use futures::SinkExt;
use futures::StreamExt;
use http::HeaderMap;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

struct NoAuth;
impl AuthProvider for NoAuth {
    fn add_auth_headers(&self, _: &mut HeaderMap) {}
}

#[tokio::test]
async fn handshake_catalog_etag_is_emitted_once_per_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, held) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let mut sockets = Vec::new();
        for connection in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut config = tungstenite::protocol::WebSocketConfig::default();
            config.extensions.permessage_deflate =
                Some(tungstenite::extensions::compression::deflate::DeflateConfig::default());
            let mut socket = tokio_tungstenite::accept_hdr_async_with_config(
                socket,
                |_: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    response.headers_mut().insert("x-models-etag", "handshake-A".parse().unwrap());
                    Ok(response)
                },
                Some(config),
            ).await.unwrap();
            for request in 0..3 {
                assert!(matches!(
                    socket.next().await.unwrap().unwrap(),
                    Message::Text(_)
                ));
                socket
                    .send(Message::Text(
                        json!({
                            "type": "response.completed",
                            "response": {"id": format!("{connection}-{request}")}
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
            sockets.push(socket);
        }
        let _ = held.await;
        drop(sockets);
    });
    let provider = Provider {
        name: "etag-test".into(),
        base_url: format!("http://{address}"),
        query_params: None,
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_retries: 0,
            base_delay: Duration::ZERO,
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_secs(5),
    };
    let client = ResponsesWebsocketClient::new(provider, Arc::new(NoAuth));
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    for connection_index in 0..2 {
        let connection = client
            .connect(&factory, HeaderMap::new(), HeaderMap::new(), None, None)
            .await
            .unwrap();
        for request_index in 0..3 {
            let request = ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest {
                model: "test".into(),
                instructions: String::new(),
                previous_response_id: None,
                input: Vec::new().into(),
                tools: None,
                tool_choice: "auto".into(),
                parallel_tool_calls: true,
                reasoning: None,
                store: false,
                stream: true,
                stream_options: None,
                include: Vec::new(),
                service_tier: None,
                prompt_cache_key: None,
                text: None,
                generate: Some(false),
                client_metadata: None,
            });
            // Do not rely on the caller's advisory connection_reused flag.
            let mut response = connection
                .stream_request(request, false, None)
                .await
                .unwrap();
            let mut etags = Vec::new();
            let mut completed = Vec::new();
            while let Some(event) = tokio::time::timeout(Duration::from_secs(5), response.next())
                .await
                .unwrap()
            {
                match event.unwrap() {
                    ResponseEvent::ModelsEtag(etag) => etags.push(etag),
                    ResponseEvent::Completed { response_id, .. } => completed.push(response_id),
                    _ => {}
                }
            }
            assert_eq!(
                etags,
                if request_index == 0 {
                    vec!["handshake-A".to_string()]
                } else {
                    Vec::new()
                }
            );
            assert_eq!(
                completed,
                vec![format!("{connection_index}-{request_index}")]
            );
        }
    }
    drop(release);
    server.await.unwrap();
}
