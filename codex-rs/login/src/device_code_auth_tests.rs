use super::*;

#[test]
fn device_code_prompt_renders_phishing_warning() {
    let prompt = device_code_prompt("https://example.com/device", "ABCD-EFGH");

    assert!(prompt.contains(
        "\x1b[90mContinue only if you started this login in Codex. If a website or another person gave you this code, cancel.\x1b[0m"
    ));
}

#[tokio::test]
async fn missing_and_zero_intervals_throttle_pending_polls() -> anyhow::Result<()> {
    for interval in [None, Some("0"), Some("1")] {
        let issuer = wiremock::MockServer::start().await;
        let mut body = serde_json::json!({"device_auth_id": "device", "user_code": "code"});
        if let Some(interval) = interval {
            body["interval"] = interval.into();
        }
        wiremock::Mock::given(wiremock::matchers::path(
            "/api/accounts/deviceauth/usercode",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
        .mount(&issuer)
        .await;
        wiremock::Mock::given(wiremock::matchers::path("/api/accounts/deviceauth/token"))
            .and(wiremock::matchers::body_json(
                serde_json::json!({"device_auth_id":"device", "user_code":"code"}),
            ))
            .respond_with(wiremock::ResponseTemplate::new(403))
            .expect(1)
            .mount(&issuer)
            .await;
        let home = tempfile::tempdir()?;
        let mut opts = ServerOptions::new(
            home.path().to_path_buf(),
            "client".into(),
            None,
            crate::AuthCredentialsStoreMode::File,
            crate::AuthKeyringBackendKind::Direct,
            crate::test_support::transport_default_auth_route_config(),
        );
        opts.issuer = issuer.uri();
        let code = request_device_code(&opts).await?;
        assert_eq!(code.interval, if interval == Some("1") { 1 } else { 5 });
        let client = create_raw_auth_client(&opts.issuer, &opts.auth_route_config)?;
        let result = poll_for_token_until(
            &client,
            &format!("{}/api/accounts", issuer.uri()),
            &code.device_auth_id,
            &code.user_code,
            code.interval,
            Instant::now() + Duration::from_millis(150),
        )
        .await;
        assert_eq!(
            result.err().expect("poll must time out").kind(),
            io::ErrorKind::TimedOut
        );
        assert!(!home.path().join("auth.json").exists());
        issuer.verify().await;
    }
    Ok(())
}

#[tokio::test]
async fn polling_deadline_covers_stalled_headers_and_bodies() -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    for send_headers in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (sent, received) = tokio::sync::oneshot::channel();
        let responder = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            assert!(socket.read(&mut bytes).await.unwrap() > 0);
            if send_headers {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{").await.unwrap();
            }
            sent.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let client = codex_http_client::HttpClientBuilder::new().build_direct()?;
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            poll_for_token_until(
                &client,
                &format!("http://{address}"),
                "device",
                "code",
                5,
                Instant::now() + Duration::from_millis(150),
            ),
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(2), received).await??;
        assert_eq!(
            result.err().expect("stalled response must time out").kind(),
            io::ErrorKind::TimedOut
        );
        responder.abort();
        assert!(responder.await.unwrap_err().is_cancelled());
    }
    Ok(())
}

#[test]
fn device_code_debug_omits_login_credentials() {
    let code = DeviceCode {
        verification_url: "https://example.com/device?secret=verification-secret".into(),
        user_code: "secret-user-code".into(),
        device_auth_id: "secret-device-id".into(),
        interval: 5,
    };
    let debug = format!("{code:?}");
    assert!(debug.contains("DeviceCode"));
    for secret in [
        &code.verification_url,
        &code.user_code,
        &code.device_auth_id,
    ] {
        assert!(!debug.contains(secret), "Debug exposed a login credential");
    }
}
