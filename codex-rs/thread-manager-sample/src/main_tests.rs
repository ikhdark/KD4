use super::*;
use serde_json::json;

fn event(value: serde_json::Value) -> EventMsg {
    serde_json::from_value(value).expect("test event should deserialize")
}

#[test]
fn events_without_item_notification_mappings_are_not_selected() {
    let unsupported_events = [
        event(json!({
            "type": "mcp_tool_call_begin",
            "call_id": "mcp-call",
            "invocation": {
                "server": "sample-server",
                "tool": "sample-tool",
                "arguments": null
            }
        })),
        event(json!({
            "type": "mcp_tool_call_end",
            "call_id": "mcp-call",
            "invocation": {
                "server": "sample-server",
                "tool": "sample-tool",
                "arguments": null
            },
            "duration": { "secs": 0, "nanos": 0 },
            "result": { "Err": "sample failure" }
        })),
        event(json!({
            "type": "patch_apply_begin",
            "call_id": "patch-call",
            "turn_id": "turn-id",
            "auto_approved": false,
            "changes": {}
        })),
    ];

    for event in unsupported_events {
        assert!(
            !is_mapped_item_event(&event),
            "unexpectedly selected {event:?}"
        );
    }
}

#[test]
fn nonfatal_diagnostics_are_rendered_for_stderr() {
    let cases = [
        (
            event(json!({ "type": "warning", "message": "degraded" })),
            "warning: degraded",
        ),
        (
            event(json!({ "type": "guardian_warning", "message": "review warning" })),
            "guardian warning: review warning",
        ),
        (
            event(json!({
                "type": "deprecation_notice",
                "summary": "old option",
                "details": "use the replacement"
            })),
            "deprecation notice: old option; use the replacement",
        ),
        (
            event(json!({
                "type": "stream_error",
                "message": "connection lost",
                "additional_details": "retrying"
            })),
            "stream error (recovering): connection lost; retrying",
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(user_facing_diagnostic(&event).as_deref(), Some(expected));
    }
}
