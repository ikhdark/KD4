use super::*;

#[test]
fn status_notification_is_the_canonical_response_payload() {
    let status = RemoteControlStatusChangedNotification {
        status: RemoteControlConnectionStatus::Connected,
        server_name: "remote.example.test".to_string(),
        installation_id: "installation-1".to_string(),
        environment_id: Some("environment-1".to_string()),
    };

    let enable = RemoteControlEnableResponse(status.clone());
    let disable = RemoteControlDisableResponse(status.clone());
    let read = RemoteControlStatusReadResponse(status.clone());

    let expected = serde_json::to_value(status).expect("serialize canonical status");
    assert_eq!(
        serde_json::to_value(enable).expect("serialize enable"),
        expected
    );
    assert_eq!(
        serde_json::to_value(disable).expect("serialize disable"),
        expected
    );
    assert_eq!(
        serde_json::to_value(read).expect("serialize read"),
        expected
    );
}
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn remote_control_clients_list_params_serialize_nullable_optional_fields() {
    assert_eq!(
        serde_json::to_value(RemoteControlClientsListParams {
            environment_id: "env-123".to_string(),
            cursor: None,
            limit: None,
            order: None,
        })
        .expect("params should serialize"),
        json!({
            "environmentId": "env-123",
            "cursor": null,
            "limit": null,
            "order": null,
        })
    );
}

#[test]
fn remote_control_clients_list_params_deserialize_camel_case_fields() {
    assert_eq!(
        serde_json::from_value::<RemoteControlClientsListParams>(json!({
            "environmentId": "env-123",
            "cursor": "cursor-123",
            "limit": 10,
            "order": "asc",
        }))
        .expect("params should deserialize"),
        RemoteControlClientsListParams {
            environment_id: "env-123".to_string(),
            cursor: Some("cursor-123".to_string()),
            limit: Some(10),
            order: Some(RemoteControlClientsListOrder::Asc),
        }
    );
}

#[test]
fn remote_control_clients_revoke_response_serializes_as_empty_object() {
    assert_eq!(
        serde_json::to_value(RemoteControlClientsRevokeResponse {})
            .expect("response should serialize"),
        json!({})
    );
}
