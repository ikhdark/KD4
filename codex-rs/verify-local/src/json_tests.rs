use super::*;
use crate::finalize::finalize_plan;
use crate::model::PlanMode;
use crate::model::SkippedDecision;

#[test]
fn legacy_json_uses_python_unicode_escaping_and_exact_newline() {
    let mut plan = PlanEnvelopeV2::new(PlanMode::Plan, "invocation");
    plan.skipped.push(SkippedDecision {
        item: "café 😀".to_string(),
        reason: "line\nfeed".to_string(),
    });
    let finalized = finalize_plan(plan, Vec::new());
    let bytes = serialize_legacy_v1(&finalized, false).expect("serialize");
    let text = String::from_utf8(bytes).expect("utf8");
    assert!(text.contains("caf\\u00e9 \\ud83d\\ude00"));
    assert!(text.ends_with("}\n"));
    assert!(!text.ends_with("}\n\n"));
}

#[test]
fn legacy_json_uses_crlf_when_requested() {
    let plan = PlanEnvelopeV2::new(PlanMode::Plan, "invocation");
    let finalized = finalize_plan(plan, Vec::new());
    let bytes = serialize_legacy_v1(&finalized, true).expect("serialize");
    assert!(bytes.ends_with(b"}\r\n"));
    assert!(!bytes.windows(1).any(|window| window == b"\n") || bytes.windows(2).any(|window| window == b"\r\n"));
}

#[test]
fn v2_raw_path_is_lossless() {
    let raw = RawPath::new(vec![0xff, b'a']);
    let encoded = serde_json::to_value(&raw).expect("serialize");
    assert!(encoded["utf8"].is_null());
    let decoded: RawPath = serde_json::from_value(encoded).expect("deserialize");
    assert_eq!(decoded.as_bytes(), &[0xff, b'a']);
}
