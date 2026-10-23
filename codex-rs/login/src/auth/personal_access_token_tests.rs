use super::*;
use crate::default_client::create_client;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn response(email: Option<&str>) -> serde_json::Value {
    json!({
        "email": email,
        "chatgpt_user_id": "user-123",
        "chatgpt_account_id": "account-123",
        "chatgpt_plan_type": "enterprise",
        "chatgpt_account_is_fedramp": true,
    })
}

#[tokio::test]
async fn hydrate_sends_bearer_token_and_preserves_optional_metadata() {
    for email in [Some("user@example.com"), None] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(WHOAMI_PATH))
            .and(header("authorization", "Bearer at-example"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response(email)))
            .expect(1)
            .mount(&server)
            .await;

        let endpoint = whoami_endpoint(&server.uri());
        let auth = hydrate_personal_access_token(
            &create_client().expect("test HTTP client"),
            &endpoint,
            "at-example",
        )
        .await
        .expect("personal access token hydration should succeed");

        assert_eq!(
            auth,
            PersonalAccessTokenAuth {
                access_token: "at-example".to_string(),
                metadata: PersonalAccessTokenMetadata {
                    email: email.map(str::to_string),
                    chatgpt_user_id: "user-123".to_string(),
                    chatgpt_account_id: "account-123".to_string(),
                    chatgpt_plan_type: "enterprise".to_string(),
                    chatgpt_account_is_fedramp: true,
                },
            }
        );
        server.verify().await;
    }
}

#[tokio::test]
async fn metadata_timeout_bounds_both_headers_and_body_stalls() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for send_headers in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}{}", listener.local_addr().unwrap(), WHOAMI_PATH);
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            if send_headers {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\n").await.unwrap();
            }
            let _ = seen_tx.send(());
            std::future::pending::<()>().await;
            drop(socket);
        });
        let client = create_client().unwrap();
        let hydration = tokio::spawn(async move {
            hydrate_personal_access_token(&client, &endpoint, "fake-test-token").await
        });
        seen_rx.await.unwrap();
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(16)).await;
        assert!(hydration.await.unwrap().is_err());
        tokio::time::resume();
        server.abort();
        let _ = server.await;
    }
}
