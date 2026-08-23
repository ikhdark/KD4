use super::*;
use anyhow::Result;
use pretty_assertions::assert_eq;
use serde_json::json;

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
