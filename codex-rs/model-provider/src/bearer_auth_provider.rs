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

