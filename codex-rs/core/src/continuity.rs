use std::collections::BTreeMap;

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

const CAPSULE_OPEN: &str = "<kd4_continuity_capsule_v1>";
const CAPSULE_CLOSE: &str = "</kd4_continuity_capsule_v1>";
const CAPSULE_HASH_DOMAIN: &[u8] = b"codex.kd4.continuity-capsule.v1";
const MAX_CAPSULE_BYTES: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContinuityCapsule {
    schema_version: u32,
    session_id: String,
    continuity_epoch: u64,
    predecessor_thread_id: Option<String>,
    working_directory: String,
    task_label: Option<String>,
    last_user_request: Option<String>,
    last_assistant_result: Option<String>,
    task_state: ContinuityTaskState,
    repository: ContinuityRepository,
    compaction: ContinuityCompaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ContinuityRepository {
    root: Option<String>,
    revision: Option<String>,
    dirty_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ContinuityTaskState {
    goal: Option<String>,
    current_state: Option<String>,
    completed_work: Option<String>,
    unresolved_work: Option<String>,
    evidence: Option<String>,
    next_action: Option<String>,
}

impl ContinuityTaskState {
    fn has_recovery_signal(&self) -> bool {
        [
            &self.goal,
            &self.current_state,
            &self.completed_work,
            &self.unresolved_work,
            &self.evidence,
            &self.next_action,
        ]
        .into_iter()
        .flatten()
        .any(|value| !value.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ContinuityCompaction {
    phase: String,
    trigger: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CanonicalContinuityEnvelope {
    schema: String,
    checkpoint_generation: String,
    core_semantic_hash: String,
    capsule: ContinuityCapsule,
}

pub(crate) enum ContinuityContextNormalization {
    Unrelated(String),
    Valid(String),
    Invalid,
}

pub(crate) fn normalize_hook_context(
    context: String,
    checkpoint_generation: &str,
) -> ContinuityContextNormalization {
    if !context.contains(CAPSULE_OPEN) && !context.contains(CAPSULE_CLOSE) {
        return ContinuityContextNormalization::Unrelated(context);
    }
    let Some(json) = marker_body(&context) else {
        return ContinuityContextNormalization::Invalid;
    };
    if json.len() > MAX_CAPSULE_BYTES || json.contains('\0') {
        return ContinuityContextNormalization::Invalid;
    }
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return ContinuityContextNormalization::Invalid;
    };
    if !capsule_shape_is_complete(&value) {
        return ContinuityContextNormalization::Invalid;
    }
    let Ok(capsule) = continuity_capsule_from_value(value) else {
        return ContinuityContextNormalization::Invalid;
    };
    if !capsule_is_complete(&capsule) {
        return ContinuityContextNormalization::Invalid;
    }
    let Ok(core_semantic_hash) = semantic_hash(&capsule) else {
        return ContinuityContextNormalization::Invalid;
    };
    let envelope = CanonicalContinuityEnvelope {
        schema: "kd4_continuity_capsule_v1".to_string(),
        checkpoint_generation: checkpoint_generation.to_string(),
        core_semantic_hash,
        capsule,
    };
    let Ok(json) = serde_json::to_string(&envelope) else {
        return ContinuityContextNormalization::Invalid;
    };
    ContinuityContextNormalization::Valid(format!("{CAPSULE_OPEN}{json}{CAPSULE_CLOSE}"))
}

fn continuity_capsule_from_value(value: Value) -> serde_json::Result<ContinuityCapsule> {
    serde_json::from_value(value)
}

pub(crate) fn deduplicate_prepared_capsules(items: &mut Vec<ResponseItem>) {
    let mut active_by_generation = BTreeMap::<String, usize>::new();
    let mut keep = vec![true; items.len()];
    for (index, item) in items.iter().enumerate() {
        let Some(text) = developer_message_text(item) else {
            continue;
        };
        if !text.contains(CAPSULE_OPEN) && !text.contains(CAPSULE_CLOSE) {
            continue;
        }
        let Some(json) = marker_body(text) else {
            tracing::warn!("invalid KD4 continuity capsule omitted from prepared history");
            keep[index] = false;
            continue;
        };
        let Ok(envelope) = serde_json::from_str::<CanonicalContinuityEnvelope>(json) else {
            tracing::warn!("invalid KD4 continuity capsule omitted from prepared history");
            keep[index] = false;
            continue;
        };
        let valid = envelope.schema == "kd4_continuity_capsule_v1"
            && capsule_is_complete(&envelope.capsule)
            && semantic_hash(&envelope.capsule)
                .is_ok_and(|identity| identity == envelope.core_semantic_hash);
        if !valid {
            tracing::warn!("invalid KD4 continuity capsule omitted from prepared history");
            keep[index] = false;
            continue;
        }
        if let Some(previous) = active_by_generation.insert(envelope.checkpoint_generation, index) {
            // Capsules are complete snapshots, so the newer valid state safely
            // replaces the prior one. This covers both identical and changed
            // semantic identities without retaining replay copies.
            keep[previous] = false;
        }
    }

    let mut position = 0usize;
    items.retain(|_| {
        let retain = keep[position];
        position = position.saturating_add(1);
        retain
    });
}

fn marker_body(text: &str) -> Option<&str> {
    let text = text.trim();
    let body = text
        .strip_prefix(CAPSULE_OPEN)?
        .strip_suffix(CAPSULE_CLOSE)?;
    (!body.contains(CAPSULE_OPEN) && !body.contains(CAPSULE_CLOSE)).then_some(body)
}

fn capsule_is_complete(capsule: &ContinuityCapsule) -> bool {
    capsule.schema_version == 1
        && !capsule.session_id.trim().is_empty()
        && !capsule.working_directory.trim().is_empty()
        && capsule.task_state.has_recovery_signal()
        && matches!(capsule.compaction.phase.as_str(), "none" | "pre" | "post")
}

fn capsule_shape_is_complete(value: &Value) -> bool {
    const SEMANTIC_FIELDS: &[&str] = &[
        "schema_version",
        "session_id",
        "continuity_epoch",
        "predecessor_thread_id",
        "working_directory",
        "task_label",
        "last_user_request",
        "last_assistant_result",
        "task_state",
        "repository",
        "compaction",
    ];
    // These fields are transport bookkeeping emitted by older hooks. They are
    // deliberately accepted but never copied into or hashed with the capsule.
    const TRANSPORT_NEUTRAL_FIELDS: &[&str] = &[
        "timestamp",
        "transcript_path",
        "last_event",
        "event_time",
        "event_times",
        "delivery_metadata",
        "temporary_delivery_metadata",
        "semantic_hash",
        "hook_semantic_hash",
    ];

    let Some(object) = value.as_object() else {
        return false;
    };
    if !has_exact_required_fields(object, SEMANTIC_FIELDS, TRANSPORT_NEUTRAL_FIELDS) {
        return false;
    }
    let Some(repository) = object.get("repository").and_then(Value::as_object) else {
        return false;
    };
    let Some(task_state) = object.get("task_state").and_then(Value::as_object) else {
        return false;
    };
    let Some(compaction) = object.get("compaction").and_then(Value::as_object) else {
        return false;
    };
    has_exact_required_fields(
        task_state,
        &[
            "goal",
            "current_state",
            "completed_work",
            "unresolved_work",
            "evidence",
            "next_action",
        ],
        &[],
    ) && has_exact_required_fields(repository, &["root", "revision", "dirty_summary"], &[])
        && has_exact_required_fields(compaction, &["phase", "trigger"], &[])
}

fn has_exact_required_fields(
    object: &Map<String, Value>,
    required: &[&str],
    allowed_extra: &[&str],
) -> bool {
    required.iter().all(|field| object.contains_key(*field))
        && object.keys().all(|field| {
            required.contains(&field.as_str()) || allowed_extra.contains(&field.as_str())
        })
}

fn semantic_hash(capsule: &ContinuityCapsule) -> serde_json::Result<String> {
    let encoded = serde_json::to_vec(capsule)?;
    let mut hasher = Sha256::new();
    hasher.update(CAPSULE_HASH_DOMAIN);
    hasher.update([0]);
    hasher.update(encoded);
    Ok(format!("{:x}", hasher.finalize()))
}

fn developer_message_text(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    if role != "developer" || content.len() != 1 {
        return None;
    }
    match &content[0] {
        ContentItem::InputText { text } => Some(text),
        ContentItem::InputImage { .. } | ContentItem::OutputText { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capsule(epoch: u64) -> String {
        format!(
            "{CAPSULE_OPEN}{{\"schema_version\":1,\"session_id\":\"session\",\"continuity_epoch\":{epoch},\"predecessor_thread_id\":null,\"working_directory\":\"repo\",\"task_label\":null,\"last_user_request\":null,\"last_assistant_result\":null,\"task_state\":{{\"goal\":\"active goal\",\"current_state\":null,\"completed_work\":null,\"unresolved_work\":null,\"evidence\":null,\"next_action\":null}},\"repository\":{{\"root\":null,\"revision\":null,\"dirty_summary\":null}},\"compaction\":{{\"phase\":\"none\",\"trigger\":null}},\"timestamp\":\"ignored\",\"semantic_hash\":\"untrusted\"}}{CAPSULE_CLOSE}"
        )
    }

    fn developer_message(text: String) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText { text }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn normalized_capsule(epoch: u64, generation: &str) -> String {
        let ContinuityContextNormalization::Valid(context) =
            normalize_hook_context(capsule(epoch), generation)
        else {
            panic!("expected valid capsule")
        };
        context
    }

    #[test]
    fn core_ignores_hook_hash_and_transport_timestamp() {
        let ContinuityContextNormalization::Valid(first) =
            normalize_hook_context(capsule(1), "root:1")
        else {
            panic!("expected valid capsule")
        };
        let without_timestamp = capsule(1).replace(",\"timestamp\":\"ignored\"", "");
        let ContinuityContextNormalization::Valid(second) =
            normalize_hook_context(without_timestamp, "root:1")
        else {
            panic!("expected valid capsule")
        };
        assert_eq!(first, second);
        assert!(!first.contains("untrusted"));
        assert!(!first.contains("timestamp"));
    }

    #[test]
    fn validated_json_value_is_the_typed_capsule_source() {
        let capsule = capsule(9);
        let json = marker_body(&capsule).expect("capsule body");
        let value = serde_json::from_str::<Value>(json).expect("validated JSON value");
        let typed = continuity_capsule_from_value(value).expect("typed capsule");

        assert_eq!(typed.continuity_epoch, 9);
        assert_eq!(typed.session_id, "session");
    }

    #[test]
    fn incomplete_capsule_is_rejected() {
        let malformed = format!(
            "{CAPSULE_OPEN}{{\"schema_version\":1,\"session_id\":\"session\"}}{CAPSULE_CLOSE}"
        );
        assert!(matches!(
            normalize_hook_context(malformed, "root:1"),
            ContinuityContextNormalization::Invalid
        ));
    }

    #[test]
    fn capsule_without_recovery_state_is_rejected() {
        let empty = capsule(1).replace("\"goal\":\"active goal\"", "\"goal\":null");
        assert!(matches!(
            normalize_hook_context(empty, "root:1"),
            ContinuityContextNormalization::Invalid
        ));
    }

    #[test]
    fn missing_optional_slot_is_still_an_incomplete_snapshot() {
        let missing_task_label = capsule(1).replace("\"task_label\":null,", "");
        assert!(matches!(
            normalize_hook_context(missing_task_label, "root:1"),
            ContinuityContextNormalization::Invalid
        ));
    }

    #[test]
    fn behavior_affecting_unknown_fields_are_rejected() {
        let unknown = capsule(1).replace(
            "\"continuity_epoch\":1,",
            "\"continuity_epoch\":1,\"resume_policy\":\"replace\",",
        );
        assert!(matches!(
            normalize_hook_context(unknown, "root:1"),
            ContinuityContextNormalization::Invalid
        ));
    }

    #[test]
    fn duplicate_and_updated_capsules_leave_one_complete_active_snapshot() {
        let older = normalized_capsule(1, "root:1");
        let duplicate = older.clone();
        let newer = normalized_capsule(2, "root:1");
        let mut items = vec![
            developer_message(older),
            developer_message(duplicate),
            developer_message(newer.clone()),
        ];
        deduplicate_prepared_capsules(&mut items);
        assert_eq!(items, vec![developer_message(newer)]);
    }

    #[test]
    fn malformed_newer_capsule_preserves_the_last_valid_snapshot() {
        let valid = normalized_capsule(1, "root:1");
        let malformed = format!("{CAPSULE_OPEN}{{not-json}}{CAPSULE_CLOSE}");
        let mut items = vec![
            developer_message(valid.clone()),
            developer_message(malformed),
        ];
        deduplicate_prepared_capsules(&mut items);
        assert_eq!(items, vec![developer_message(valid)]);
    }
}
