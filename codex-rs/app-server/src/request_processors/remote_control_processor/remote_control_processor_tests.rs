use super::*;
use crate::error_code::INTERNAL_ERROR_CODE;
use crate::error_code::INVALID_REQUEST_ERROR_CODE;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn pairing_start_returns_internal_error_when_remote_control_is_unavailable() {
    let err = RemoteControlRequestProcessor::new(/*remote_control_handle*/ None)
        .pairing_start(
            RemoteControlPairingStartParams::default(),
            /*app_server_client_name*/ None,
        )
        .await
        .expect_err("missing remote control should fail pairing");

    assert_eq!(
        err,
        JSONRPCErrorError {
            code: INTERNAL_ERROR_CODE,
            data: None,
            message: "remote control is unavailable for this app-server".to_string(),
        }
    );
}

#[tokio::test]
async fn pairing_status_validates_codes_before_resolving_handle() {
    let processor = RemoteControlRequestProcessor::new(/*remote_control_handle*/ None);
    for (pairing_code, manual_pairing_code, code, message) in [
        (
            Some("pairing-code"),
            None,
            INTERNAL_ERROR_CODE,
            "remote control is unavailable for this app-server",
        ),
        (
            None,
            Some("ABCD-EFGH"),
            INTERNAL_ERROR_CODE,
            "remote control is unavailable for this app-server",
        ),
        (
            None,
            None,
            INVALID_REQUEST_ERROR_CODE,
            "remoteControl/pairing/status requires pairingCode or manualPairingCode",
        ),
        (
            Some("pairing-code"),
            Some("ABCD-EFGH"),
            INVALID_REQUEST_ERROR_CODE,
            "remoteControl/pairing/status accepts either pairingCode or manualPairingCode, not both",
        ),
    ] {
        assert_eq!(
            processor
                .pairing_status(RemoteControlPairingStatusParams {
                    pairing_code: pairing_code.map(str::to_string),
                    manual_pairing_code: manual_pairing_code.map(str::to_string),
                })
                .await,
            Err(JSONRPCErrorError {
                code,
                data: None,
                message: message.to_string(),
            }),
            "pairing_code={pairing_code:?}, manual_pairing_code={manual_pairing_code:?}"
        );
    }
}

#[test]
fn pairing_and_client_management_classify_errors_by_operation() {
    for (kind, pairing_code, client_code) in [
        (
            io::ErrorKind::InvalidInput,
            INVALID_REQUEST_ERROR_CODE,
            INVALID_REQUEST_ERROR_CODE,
        ),
        (
            io::ErrorKind::NotFound,
            INTERNAL_ERROR_CODE,
            INVALID_REQUEST_ERROR_CODE,
        ),
        (
            io::ErrorKind::PermissionDenied,
            INTERNAL_ERROR_CODE,
            INVALID_REQUEST_ERROR_CODE,
        ),
        (
            io::ErrorKind::WouldBlock,
            INTERNAL_ERROR_CODE,
            INVALID_REQUEST_ERROR_CODE,
        ),
        (
            io::ErrorKind::Other,
            INTERNAL_ERROR_CODE,
            INTERNAL_ERROR_CODE,
        ),
    ] {
        for (actual, code) in [
            (
                map_pairing_start_error(io::Error::new(kind, "pairing unavailable")),
                pairing_code,
            ),
            (
                map_client_management_error(io::Error::new(kind, "pairing unavailable")),
                client_code,
            ),
        ] {
            assert_eq!(
                actual,
                JSONRPCErrorError {
                    code,
                    data: None,
                    message: "pairing unavailable".to_string(),
                },
                "{kind:?}"
            );
        }
    }
}
