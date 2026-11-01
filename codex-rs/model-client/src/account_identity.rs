/// Recovery identity resolved for the dispatched attempt. Do not derive Debug: API keys
/// are compared only in memory and must never appear in retry diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub enum AccountChangeIdentity {
    Account(String),
    ApiKey(String),
    Headers(Vec<(String, Vec<u8>)>),
}

impl AccountChangeIdentity {
    pub fn from_auth(auth: &codex_login::CodexAuth) -> Option<Self> {
        if let codex_login::CodexAuth::Headers(headers) = auth {
            return Some(headers.headers().get("chatgpt-account-id")
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .map(|account| Self::Account(account.to_owned()))
                .unwrap_or_else(|| {
                    let mut values = headers.headers().iter()
                        .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
                        .collect::<Vec<_>>();
                    values.sort_unstable();
                    Self::Headers(values)
                }));
        }
        // Refreshing tokens for the same account does not reset its usage limit.
        auth.get_account_id()
            .map(Self::Account)
            .or_else(|| auth.api_key().map(|key| Self::ApiKey(key.to_owned())))
    }
}
