use codex_protocol::protocol::EventMsg;
use sha2::Digest;
use sha2::Sha256;

const TERMINAL_EVENT_FINGERPRINT_V1: &str = "CODEX_TERMINAL_EVENT_FINGERPRINT_V1";

/// Returns a stable fingerprint for a terminal event's semantic payload.
///
/// The outer [`codex_protocol::protocol::Event::id`] is intentionally excluded: callers bind
/// the fingerprint to an exact turn separately. This lets core, recovery, and app-server compare
/// the exact terminal decision without coupling the hash to transport metadata.
pub fn terminal_event_fingerprint(event: &EventMsg) -> Option<String> {
    if !matches!(event, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)) {
        return None;
    }
    let encoded = serde_json::to_vec(event).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(TERMINAL_EVENT_FINGERPRINT_V1.as_bytes());
    hasher.update([0]);
    hasher.update(encoded);
    Some(format!("{:x}", hasher.finalize()))
}
