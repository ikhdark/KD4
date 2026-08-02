use super::REALTIME_INSTRUCTIONS_TOKEN_BUDGET;
use super::RealtimeHandoffState;
use super::RealtimeSessionKind;
use super::join_realtime_instructions;
use super::realtime_delegation_from_handoff;
use super::realtime_request_headers;
use super::realtime_text_from_handoff_request;
use super::remaining_realtime_startup_context_budget;
use super::wrap_realtime_delegation_input;
use crate::realtime_context::approx_realtime_token_count;
use async_channel::bounded;
use codex_config::config_toml::RealtimeWsVersion;
use codex_protocol::protocol::RealtimeHandoffRequested;
use codex_protocol::protocol::RealtimeTranscriptEntry;
use pretty_assertions::assert_eq;

#[test]
fn realtime_instruction_join_preserves_separator_for_short_inputs() {
    assert_eq!(
        join_realtime_instructions("backend", "<startup_context>context</startup_context>"),
        "backend\n\n<startup_context>context</startup_context>"
    );
    assert_eq!(
        remaining_realtime_startup_context_budget(""),
        REALTIME_INSTRUCTIONS_TOKEN_BUDGET
    );
    assert_eq!(
        remaining_realtime_startup_context_budget("abc"),
        REALTIME_INSTRUCTIONS_TOKEN_BUDGET - 2
    );
}

#[test]
fn realtime_instruction_join_enforces_total_budget() {
    let backend = format!("backend-start {} backend-end", "x".repeat(12_000));
    let startup_context = format!(
        "<startup_context>\nstartup-start {} startup-end\n</startup_context>",
        "y".repeat(50_000)
    );
    let instructions = join_realtime_instructions(&backend, &startup_context);

    assert!(approx_realtime_token_count(&instructions) <= REALTIME_INSTRUCTIONS_TOKEN_BUDGET);
    assert!(instructions.contains("backend-start"));
    assert!(instructions.contains("backend-end"));
    assert!(instructions.contains("<startup_context>"));
    assert!(instructions.contains("</startup_context>"));
    assert!(instructions.contains("tokens truncated"));
}

#[test]
fn prefers_handoff_input_transcript_over_active_transcript() {
    let handoff = RealtimeHandoffRequested {
        handoff_id: "handoff_1".to_string(),
        item_id: "item_1".to_string(),
        input_transcript: "ignored".to_string(),
        active_transcript: vec![
            RealtimeTranscriptEntry {
                role: "user".to_string(),
                text: "hello".to_string(),
            },
            RealtimeTranscriptEntry {
                role: "assistant".to_string(),
                text: "hi there".to_string(),
            },
        ],
    };
    assert_eq!(
        realtime_text_from_handoff_request(&handoff),
        Some("ignored".to_string())
    );
}

#[test]
fn extracts_text_from_handoff_request_active_transcript_if_input_missing() {
    let handoff = RealtimeHandoffRequested {
        handoff_id: "handoff_1".to_string(),
        item_id: "item_1".to_string(),
        input_transcript: String::new(),
        active_transcript: vec![RealtimeTranscriptEntry {
            role: "user".to_string(),
            text: "hello".to_string(),
        }],
    };
    assert_eq!(
        realtime_text_from_handoff_request(&handoff),
        Some("user: hello".to_string())
    );
}

#[test]
fn wraps_handoff_with_transcript_delta() {
    let handoff = RealtimeHandoffRequested {
        handoff_id: "handoff_1".to_string(),
        item_id: "item_1".to_string(),
        input_transcript: "delegate this".to_string(),
        active_transcript: vec![
            RealtimeTranscriptEntry {
                role: "user".to_string(),
                text: "hello".to_string(),
            },
            RealtimeTranscriptEntry {
                role: "assistant".to_string(),
                text: "hi there".to_string(),
            },
        ],
    };
    assert_eq!(
        realtime_delegation_from_handoff(&handoff),
        Some(
            "<realtime_delegation>\n  <input>delegate this</input>\n  <transcript_delta>user: hello\nassistant: hi there</transcript_delta>\n</realtime_delegation>"
                .to_string()
        )
    );
}

#[test]
fn extracts_text_from_handoff_request_input_transcript_if_messages_missing() {
    let handoff = RealtimeHandoffRequested {
        handoff_id: "handoff_1".to_string(),
        item_id: "item_1".to_string(),
        input_transcript: "ignored".to_string(),
        active_transcript: vec![],
    };
    assert_eq!(
        realtime_text_from_handoff_request(&handoff),
        Some("ignored".to_string())
    );
}

#[test]
fn ignores_empty_handoff_request_input_transcript() {
    let handoff = RealtimeHandoffRequested {
        handoff_id: "handoff_1".to_string(),
        item_id: "item_1".to_string(),
        input_transcript: String::new(),
        active_transcript: vec![],
    };
    assert_eq!(realtime_text_from_handoff_request(&handoff), None);
}

#[test]
fn wraps_realtime_delegation_input() {
    assert_eq!(
        wrap_realtime_delegation_input("hello", /*transcript_delta*/ None),
        "<realtime_delegation>\n  <input>hello</input>\n</realtime_delegation>"
    );
}

#[test]
fn wraps_realtime_delegation_input_with_xml_escaping() {
    assert_eq!(
        wrap_realtime_delegation_input("use a < b && c > d", Some("saw <that>")),
        "<realtime_delegation>\n  <input>use a &lt; b &amp;&amp; c &gt; d</input>\n  <transcript_delta>saw &lt;that&gt;</transcript_delta>\n</realtime_delegation>"
    );
}

#[test]
fn wraps_realtime_delegation_input_with_xml_escaping_without_transcript() {
    assert_eq!(
        wrap_realtime_delegation_input("use a < b && c > d", /*transcript_delta*/ None),
        "<realtime_delegation>\n  <input>use a &lt; b &amp;&amp; c &gt; d</input>\n</realtime_delegation>"
    );
}

#[tokio::test]
async fn clears_active_handoff_explicitly() {
    let (tx, _rx) = bounded(1);
    let state = RealtimeHandoffState::new(
        tx,
        /*client_managed_handoffs*/ false,
        /*codex_responses_as_items*/ false,
        /*codex_response_item_prefix*/ None,
        /*codex_response_handoff_prefix*/ None,
        RealtimeSessionKind::V1,
    );

    *state.active_handoff.lock().await = Some("handoff_1".to_string());
    assert_eq!(
        state.active_handoff.lock().await.clone(),
        Some("handoff_1".to_string())
    );

    *state.active_handoff.lock().await = None;
    assert_eq!(state.active_handoff.lock().await.clone(), None);
}

#[test]
fn uses_quicksilver_alpha_header_for_realtime_v1() {
    let headers = realtime_request_headers(
        Some("session_1"),
        Some("sk-test"),
        RealtimeWsVersion::V1,
        "codex_work_desktop",
    )
    .expect("headers")
    .expect("headers");

    assert_eq!(
        headers
            .get("openai-alpha")
            .and_then(|value| value.to_str().ok()),
        Some("quicksilver=v1")
    );
}

#[test]
fn omits_quicksilver_alpha_header_for_realtime_v2() {
    let headers = realtime_request_headers(
        Some("session_1"),
        Some("sk-test"),
        RealtimeWsVersion::V2,
        "codex_work_desktop",
    )
    .expect("headers")
    .expect("headers");

    assert!(headers.get("openai-alpha").is_none());
}

#[test]
fn realtime_headers_include_only_non_default_originator() {
    let default_originator = codex_login::default_client::originator();
    for (originator, expected_header) in [
        ("codex_work_desktop", Some("codex_work_desktop")),
        (default_originator.value.as_str(), None),
    ] {
        let headers = realtime_request_headers(
            Some("session_1"),
            Some("sk-test"),
            RealtimeWsVersion::V2,
            originator,
        )
        .expect("headers")
        .expect("headers");

        assert_eq!(
            headers
                .get("originator")
                .and_then(|value| value.to_str().ok()),
            expected_header
        );
    }
}
