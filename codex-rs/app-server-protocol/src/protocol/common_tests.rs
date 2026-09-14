use super::*;
use anyhow::Result;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn nonempty_response_preserves_json_and_retained_payload() -> Result<()> {
    let response = v2::CollaborationModeListResponse {
        data: vec![v2::CollaborationModeMask {
            name: "custom".into(),
            mode: None,
            model: Some("model-1".into()),
            reasoning_effort: Some(None),
        }],
    };
    let (id, result, payload) = ClientResponsePayload::CollaborationModeList(response)
        .into_jsonrpc_parts_and_payload(RequestId::Integer(9))?;
    assert_eq!(id, RequestId::Integer(9));
    assert_eq!(
        result,
        json!({"data": [{"name": "custom", "mode": null, "model": "model-1", "reasoning_effort": null}]})
    );
    let Some(ClientResponse::CollaborationModeList {
        request_id,
        response,
    }) = payload.and_then(|payload| payload.into_client_response(RequestId::Integer(9)))
    else {
        panic!("expected collaboration mode response");
    };
    assert_eq!(request_id, RequestId::Integer(9));
    assert_eq!(
        response,
        v2::CollaborationModeListResponse {
            data: vec![v2::CollaborationModeMask {
                name: "custom".into(),
                mode: None,
                model: Some("model-1".into()),
                reasoning_effort: Some(None),
            }],
        }
    );
    Ok(())
}

#[test]
fn credentials_are_redacted_from_debug_but_preserved_on_wire() {
    fn check(value: impl std::fmt::Debug + serde::Serialize, field: &str) {
        let debug = format!("{value:?}");
        assert!(!debug.contains("sentinel-secret"));
        assert!(debug.contains("[REDACTED]"));
        assert_eq!(
            serde_json::to_value(value).unwrap()[field],
            "sentinel-secret"
        );
    }
    check(
        v2::LoginAccountParams::ApiKey {
            api_key: "sentinel-secret".into(),
        },
        "apiKey",
    );
    check(
        v2::LoginAccountParams::ChatgptAuthTokens {
            access_token: "sentinel-secret".into(),
            chatgpt_account_id: "account".into(),
            chatgpt_plan_type: None,
        },
        "accessToken",
    );
    check(
        v2::ChatgptAuthTokensRefreshResponse {
            access_token: "sentinel-secret".into(),
            chatgpt_account_id: "account".into(),
            chatgpt_plan_type: None,
        },
        "accessToken",
    );
    check(
        v1::LoginApiKeyParams {
            api_key: "sentinel-secret".into(),
        },
        "apiKey",
    );
    check(
        v1::GetAuthStatusResponse {
            auth_method: None,
            auth_token: Some("sentinel-secret".into()),
            requires_openai_auth: Some(false),
        },
        "authToken",
    );
    check(
        v2::AttestationGenerateResponse {
            token: "sentinel-secret".into(),
        },
        "token",
    );
}

#[test]
fn client_response_payload_returns_jsonrpc_parts_and_client_response() -> Result<()> {
    let (request_id, result, payload) =
        ClientResponsePayload::ThreadArchive(v2::ThreadArchiveResponse {})
            .into_jsonrpc_parts_and_payload(RequestId::Integer(7))?;

    assert_eq!(request_id, RequestId::Integer(7));
    assert_eq!(result, json!({}));

    let Some(ClientResponse::ThreadArchive {
        request_id,
        response: _,
    }) = payload.and_then(|payload| payload.into_client_response(RequestId::Integer(7)))
    else {
        panic!("expected thread/archive client response");
    };
    assert_eq!(request_id, RequestId::Integer(7));
    Ok(())
}

#[test]
fn turn_interrupt_payload_returns_typed_client_response() -> Result<()> {
    let (request_id, result, payload) =
        ClientResponsePayload::TurnInterrupt(v2::TurnInterruptResponse {})
            .into_jsonrpc_parts_and_payload(RequestId::Integer(8))?;

    assert_eq!(request_id, RequestId::Integer(8));
    assert_eq!(result, json!({}));
    let Some(ClientResponse::TurnInterrupt {
        request_id,
        response: _,
    }) = payload.and_then(|payload| payload.into_client_response(RequestId::Integer(8)))
    else {
        panic!("expected turn/interrupt client response");
    };
    assert_eq!(request_id, RequestId::Integer(8));
    Ok(())
}
