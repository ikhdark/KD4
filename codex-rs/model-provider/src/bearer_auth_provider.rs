use codex_api::AuthError;
use codex_api::AuthProvider;
use http::HeaderMap;
use http::HeaderValue;

/// Bearer-token auth provider for OpenAI-compatible model-provider requests.
#[derive(Clone, Default)]
pub struct BearerAuthProvider {
    pub token: Option<String>,
    pub account_id: Option<String>,
    pub is_fedramp_account: bool,
}

impl BearerAuthProvider {
    pub fn new(token: String) -> Self {
        Self {
            token: Some(token),
            account_id: None,
            is_fedramp_account: false,
        }
    }

    pub fn for_test(token: Option<&str>, account_id: Option<&str>) -> Self {
        Self {
            token: token.map(str::to_string),
            account_id: account_id.map(str::to_string),
            is_fedramp_account: false,
        }
    }
}

impl AuthProvider for BearerAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        let _ = self.try_add_auth_headers(headers);
    }

    fn try_add_auth_headers(&self, headers: &mut HeaderMap) -> Result<(), AuthError> {
        let authorization = self
            .token
            .as_ref()
            .map(|token| {
                let mut value =
                    HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                        AuthError::Build("invalid bearer authorization header".to_string())
                    })?;
                value.set_sensitive(true);
                Ok::<_, AuthError>(value)
            })
            .transpose()?;
        let account = self
            .account_id
            .as_ref()
            .map(|account| {
                HeaderValue::from_str(account)
                    .map_err(|_| AuthError::Build("invalid account header".to_string()))
            })
            .transpose()?;
        if let Some(value) = authorization {
            headers.insert(http::header::AUTHORIZATION, value);
        }
        if let Some(value) = account {
            headers.insert("ChatGPT-Account-ID", value);
        }
        if self.is_fedramp_account {
            headers.insert("X-OpenAI-Fedramp", HeaderValue::from_static("true"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn bearer_auth_provider_reports_when_auth_header_will_attach() {
        let auth = BearerAuthProvider {
            token: Some("access-token".to_string()),
            account_id: None,
            is_fedramp_account: false,
        };

        assert_eq!(
            codex_api::auth_header_telemetry(&auth),
            codex_api::AuthHeaderTelemetry {
                attached: true,
                name: Some("authorization"),
            }
        );
    }

    #[test]
    fn bearer_auth_provider_adds_auth_headers() {
        let auth = BearerAuthProvider::for_test(Some("access-token"), Some("workspace-123"));
        let mut headers = HeaderMap::new();

        auth.add_auth_headers(&mut headers);

        assert_eq!(
            headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer access-token")
        );
        assert!(headers[http::header::AUTHORIZATION].is_sensitive());
        assert_eq!(
            headers
                .get("ChatGPT-Account-ID")
                .and_then(|value| value.to_str().ok()),
            Some("workspace-123")
        );
    }

    #[test]
    fn bearer_auth_provider_adds_fedramp_routing_header_for_fedramp_accounts() {
        let auth = BearerAuthProvider {
            token: Some("access-token".to_string()),
            account_id: Some("workspace-123".to_string()),
            is_fedramp_account: true,
        };
        let mut headers = HeaderMap::new();

        auth.add_auth_headers(&mut headers);

        assert_eq!(
            headers
                .get("X-OpenAI-Fedramp")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
    }
    #[tokio::test]
    async fn malformed_credentials_reject_request_dispatch() {
        for auth in [
            BearerAuthProvider::for_test(Some("bad\nsecret"), None),
            BearerAuthProvider::for_test(Some("token"), Some("bad\naccount")),
        ] {
            let request = codex_http_client::Request::new(
                http::Method::GET,
                "https://example.com".to_string(),
            );
            assert!(matches!(
                auth.apply_auth(request).await,
                Err(AuthError::Build(_))
            ));
        }
        let request =
            codex_http_client::Request::new(http::Method::GET, "https://example.com".to_string());
        assert!(
            BearerAuthProvider::default()
                .apply_auth(request)
                .await
                .unwrap()
                .headers
                .is_empty()
        );
    }
}
