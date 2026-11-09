// Token endpoints may echo credentials in arbitrary messages. Only reviewed
// OAuth codes are suitable for errors that callers can send to diagnostics.
pub(crate) fn try_parse_error_message(text: &str) -> String {
    let json = serde_json::from_str::<serde_json::Value>(text).unwrap_or_default();
    let error = json.get("error");
    let code = error.and_then(|value| value.as_str().or_else(|| value.get("code")?.as_str()));
    match code {
        Some(code @ ("invalid_request" | "invalid_client" | "invalid_grant"
            | "unauthorized_client" | "unsupported_grant_type" | "invalid_scope"
            | "server_error" | "temporarily_unavailable")) => code.to_string(),
        _ => "OAuth endpoint rejected the request (response details withheld)".to_string(),
    }
}
