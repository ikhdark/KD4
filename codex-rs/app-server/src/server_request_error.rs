use codex_app_server_protocol::JSONRPCErrorError;

pub(crate) const TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON: &str = "turnTransition";

pub(crate) fn is_turn_transition_server_request_error(error: &JSONRPCErrorError) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(serde_json::Value::as_str)
        == Some(TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON)
}

#[cfg(test)]
mod tests {
    use super::is_turn_transition_server_request_error;
    use codex_app_server_protocol::JSONRPCErrorError;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn turn_transition_error_is_detected() {
        for (data, expected) in [
            (Some(json!({ "reason": "turnTransition" })), true),
            (Some(json!({ "reason": "other" })), false),
            (Some(json!({ "reason": null })), false),
            (Some(json!({ "reason": true })), false),
            (Some(json!({})), false),
            (Some(json!("turnTransition")), false),
            (None, false),
        ] {
            // Classification must depend on structured data, not human-readable
            // text or a generic error code shared by unrelated failures.
            for message in [
                "boom",
                "client request resolved because the turn state was changed",
            ] {
                let error = JSONRPCErrorError {
                    code: -1,
                    message: message.to_string(),
                    data: data.clone(),
                };
                assert_eq!(
                    is_turn_transition_server_request_error(&error),
                    expected,
                    "{error:?}"
                );
            }
        }
    }
}
