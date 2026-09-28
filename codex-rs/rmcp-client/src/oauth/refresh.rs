use std::future::Future;
use std::path::PathBuf;

use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use oauth2::HttpRequest;
use oauth2::HttpResponse;
use oauth2::TokenResponse;
use rmcp::transport::auth::OAuthHttpClientError;
use rmcp::transport::auth::OAuthTokenResponse;

use super::StoredOAuthTokens;
use super::WrappedOAuthTokenResponse;
use super::compute_expires_at_millis;
use super::compute_store_key;
use super::load_oauth_tokens;
use super::save_oauth_tokens;
use super::store_lock::OAuthStoreLock;
use super::token_needs_refresh;

#[derive(Clone)]
pub(crate) struct RefreshContext {
    pub codex_home: PathBuf,
    pub server_name: String,
    pub server_url: String,
    pub client_id: String,
    pub store_mode: OAuthCredentialsStoreMode,
    pub keyring_backend_kind: AuthKeyringBackendKind,
}

impl RefreshContext {
    pub(crate) async fn execute<F, Fut>(
        &self,
        mut request: HttpRequest,
        send: F,
    ) -> Result<HttpResponse, OAuthHttpClientError>
    where
        F: FnOnce(HttpRequest) -> Fut,
        Fut: Future<Output = Result<HttpResponse, OAuthHttpClientError>>,
    {
        let mut form: Vec<(String, String)> = url::form_urlencoded::parse(request.body())
            .into_owned()
            .collect();
        if request.method() != http::Method::POST
            || !form
                .iter()
                .any(|(key, value)| key == "grant_type" && value == "refresh_token")
        {
            return send(request).await;
        }
        let Some(refresh_index) = form.iter().position(|(key, _)| key == "refresh_token") else {
            return send(request).await;
        };
        let context = self.clone();
        // Lock order is credential-refresh -> aggregate-store. Never hold the
        // aggregate store lock across network I/O or share environment HTTP clients.
        let (guard, stored) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let key = compute_store_key(&context.server_name, &context.server_url)?;
            let guard = OAuthStoreLock::acquire_refresh(&context.codex_home, &key)?;
            let stored = load_oauth_tokens(
                &context.codex_home,
                &context.server_name,
                &context.server_url,
                context.store_mode,
                context.keyring_backend_kind,
            )?;
            Ok((guard, stored))
        })
        .await
        .map_err(error)?
        .map_err(error)?;
        if let Some(stored) = stored {
            if stored.client_id != self.client_id {
                return Err(OAuthHttpClientError::new(
                    "OAuth client registration changed; reconnect before refreshing",
                ));
            }
            let refresh = stored
                .token_response
                .0
                .refresh_token()
                .map(oauth2::RefreshToken::secret);
            if let Some(refresh) = refresh
                && refresh != &form[refresh_index].1
            {
                if !token_needs_refresh(stored.expires_at) {
                    let body = serde_json::to_vec(&stored.token_response.0).map_err(error)?;
                    return http::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(body)
                        .map_err(error);
                }
                // A long-idle client may miss more than one rotation.
                form[refresh_index].1 = refresh.clone();
                *request.body_mut() = url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(&form)
                    .finish()
                    .into_bytes();
                request.headers_mut().remove(http::header::CONTENT_LENGTH);
            }
        }
        let mut response = send(request).await?;
        if response.status().is_success()
            && let Ok(mut tokens) = serde_json::from_slice::<OAuthTokenResponse>(response.body())
        {
            if tokens.refresh_token().is_none() {
                tokens.set_refresh_token(Some(oauth2::RefreshToken::new(
                    form[refresh_index].1.clone(),
                )));
                *response.body_mut() = serde_json::to_vec(&tokens).map_err(error)?;
            }
            let stored = StoredOAuthTokens {
                server_name: self.server_name.clone(),
                url: self.server_url.clone(),
                client_id: self.client_id.clone(),
                expires_at: compute_expires_at_millis(&tokens),
                token_response: WrappedOAuthTokenResponse(tokens),
            };
            let context = self.clone();
            // The lock and durable write survive a cancelled waiter. Only this
            // coordinator persists refreshed tokens, never a stale manager snapshot.
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let _guard = guard;
                save_oauth_tokens(
                    &context.codex_home,
                    &context.server_name,
                    &stored,
                    context.store_mode,
                    context.keyring_backend_kind,
                )?;
                Ok(())
            })
            .await
            .map_err(error)?
            .map_err(error)?;
        }
        Ok(response)
    }
}

fn error(error: impl std::fmt::Display) -> OAuthHttpClientError {
    OAuthHttpClientError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn cancelled_refresh_keeps_the_durable_rotation_without_blocking_executor() {
        let home = tempfile::tempdir().unwrap();
        let tokens: StoredOAuthTokens = serde_json::from_value(serde_json::json!({
            "server_name":"test", "url":"https://example.test/mcp", "client_id":"client",
            "token_response":{"access_token":"old","refresh_token":"old-refresh","token_type":"bearer"},
            "expires_at":0
        })).unwrap();
        save_oauth_tokens(
            home.path(),
            "test",
            &tokens,
            OAuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )
        .unwrap();
        let context = RefreshContext {
            codex_home: home.path().to_path_buf(),
            server_name: tokens.server_name.clone(),
            server_url: tokens.url.clone(),
            client_id: tokens.client_id.clone(),
            store_mode: OAuthCredentialsStoreMode::File,
            keyring_backend_kind: AuthKeyringBackendKind::Direct,
        };
        let (blocked_tx, blocked_rx) = tokio::sync::oneshot::channel();
        let running = tokio::spawn({
            let context = context.clone();
            async move {
                let request = http::Request::builder()
                    .method("POST")
                    .uri("https://example.test/token")
                    .body(b"grant_type=refresh_token&refresh_token=old-refresh".to_vec())
                    .unwrap();
                let lock_path = context.codex_home.join("mcp-oauth-locks/file-store.lock");
                context.execute(request, |_| async move {
                    let file = std::fs::OpenOptions::new().read(true).write(true).open(lock_path).unwrap();
                    file.try_lock().unwrap();
                    blocked_tx.send(file).unwrap();
                    Ok(http::Response::builder().status(200).body(
                        br#"{"access_token":"new","refresh_token":"rotated","token_type":"bearer","expires_in":3600}"#.to_vec()
                    ).unwrap())
                }).await
            }
        });
        // On this current-thread runtime, receipt means execute has reached the
        // blocking write's await after accepting the response and spawning it.
        let held_store_lock = blocked_rx.await.unwrap();
        assert!(!running.is_finished());
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        drop(held_store_lock);
        let stored = tokio::task::spawn_blocking(move || {
            let key = compute_store_key("test", &context.server_url).unwrap();
            let _guard = OAuthStoreLock::acquire_refresh(&context.codex_home, &key).unwrap();
            load_oauth_tokens(
                &context.codex_home,
                "test",
                &context.server_url,
                context.store_mode,
                context.keyring_backend_kind,
            )
            .unwrap()
            .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(stored.token_response.0.access_token().secret(), "new");
        assert_eq!(
            stored.token_response.0.refresh_token().unwrap().secret(),
            "rotated"
        );
    }

    #[tokio::test]
    async fn stale_refresh_uses_latest_token_and_rejects_changed_registration() {
        let home = tempfile::tempdir().unwrap();
        let mut stored: StoredOAuthTokens = serde_json::from_value(serde_json::json!({
            "server_name":"test", "url":"https://example.test/mcp", "client_id":"client",
            "token_response":{"access_token":"expired","refresh_token":"latest-refresh","token_type":"bearer"},
            "expires_at":0
        })).unwrap();
        let context = RefreshContext {
            codex_home: home.path().to_path_buf(),
            server_name: stored.server_name.clone(),
            server_url: stored.url.clone(),
            client_id: stored.client_id.clone(),
            store_mode: OAuthCredentialsStoreMode::File,
            keyring_backend_kind: AuthKeyringBackendKind::Direct,
        };
        let save = |stored: &StoredOAuthTokens| {
            save_oauth_tokens(
                home.path(),
                "test",
                stored,
                context.store_mode,
                context.keyring_backend_kind,
            )
            .unwrap();
        };
        let request = || {
            http::Request::builder()
                .method("POST")
                .uri("https://example.test/token")
                .header("content-length", "56")
                .body(b"grant_type=refresh_token&refresh_token=old-refresh".to_vec())
                .unwrap()
        };
        save(&stored);
        let response =
            context
                .execute(request(), |request| async move {
                    assert!(
                        String::from_utf8_lossy(request.body())
                            .contains("refresh_token=latest-refresh")
                    );
                    assert!(!request.headers().contains_key(http::header::CONTENT_LENGTH));
                    Ok(http::Response::builder().status(200).body(
                br#"{"access_token":"renewed","token_type":"bearer","expires_in":3600}"#.to_vec()
            ).unwrap())
                })
                .await
                .unwrap();
        let response: OAuthTokenResponse = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(response.refresh_token().unwrap().secret(), "latest-refresh");
        stored = load_oauth_tokens(
            home.path(),
            "test",
            &context.server_url,
            context.store_mode,
            context.keyring_backend_kind,
        )
        .unwrap()
        .unwrap();
        assert_eq!(stored.token_response.0.access_token().secret(), "renewed");
        assert_eq!(
            stored.token_response.0.refresh_token().unwrap().secret(),
            "latest-refresh"
        );
        stored.client_id = "different-registration".into();
        save(&stored);
        let result = context
            .execute(request(), |_| async {
                panic!("must not send credentials for a different registration");
            })
            .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("registration changed")
        );
        let unchanged = load_oauth_tokens(
            home.path(),
            "test",
            &context.server_url,
            context.store_mode,
            context.keyring_backend_kind,
        )
        .unwrap()
        .unwrap();
        assert_eq!(unchanged.client_id, "different-registration");
        assert_eq!(
            unchanged.token_response.0.access_token().secret(),
            "renewed"
        );
    }

    #[tokio::test]
    async fn concurrent_refreshes_reuse_durable_rotation() {
        let home = tempfile::tempdir().unwrap();
        let tokens: StoredOAuthTokens = serde_json::from_value(serde_json::json!({
            "server_name":"test", "url":"https://example.test/mcp", "client_id":"client",
            "token_response":{"access_token":"old","refresh_token":"old-refresh","token_type":"bearer"},
            "expires_at":0
        })).unwrap();
        save_oauth_tokens(
            home.path(),
            "test",
            &tokens,
            OAuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )
        .unwrap();
        let context = RefreshContext {
            codex_home: home.path().to_path_buf(),
            server_name: tokens.server_name.clone(),
            server_url: tokens.url.clone(),
            client_id: tokens.client_id.clone(),
            store_mode: OAuthCredentialsStoreMode::File,
            keyring_backend_kind: AuthKeyringBackendKind::Direct,
        };
        let second = context.clone();
        let request = || {
            http::Request::builder()
                .method("POST")
                .uri("https://example.test/token")
                .body(b"grant_type=refresh_token&refresh_token=old-refresh".to_vec())
                .unwrap()
        };
        let calls = AtomicUsize::new(0);
        let send = |request: HttpRequest| {
            assert!(String::from_utf8_lossy(request.body()).contains("old-refresh"));
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                Ok(http::Response::builder().status(200).body(serde_json::to_vec(&serde_json::json!({
                    "access_token":"new","refresh_token":"rotated","token_type":"bearer","expires_in":3600
                })).unwrap()).unwrap())
            }
        };
        let (first, second) = tokio::join!(
            context.execute(request(), send),
            second.execute(request(), send)
        );
        for response in [first.unwrap(), second.unwrap()] {
            let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
            assert_eq!(body["access_token"], "new");
            assert_eq!(body["refresh_token"], "rotated");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let persisted = load_oauth_tokens(
            home.path(),
            "test",
            &context.server_url,
            context.store_mode,
            context.keyring_backend_kind,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            persisted.token_response.0.refresh_token().unwrap().secret(),
            "rotated"
        );
    }
}
