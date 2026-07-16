use super::*;
use crate::finalize::finalize_plan;
use crate::model::CommandResultV2;
use crate::model::CommandSpecV2;
use crate::model::DirtyGroup;
use crate::model::LogState;
use crate::model::PlanMode;
use crate::model::SkippedDecision;
use crate::model::Verdict;

const FROZEN_EMPTY_PLAN_LF: &[u8] = b"{\n  \"schema_version\": 1,\n  \"producer\": \"kd4.verify_local\",\n  \"mode\": \"plan\",\n  \"scope\": null,\n  \"planned\": [],\n  \"skipped\": [],\n  \"results\": [],\n  \"cached\": [],\n  \"quarantined_failures\": [],\n  \"rerun\": null,\n  \"cache_miss_reasons\": [],\n  \"verdict\": \"PLANNED\"\n}\n";

const FROZEN_POPULATED_LF: &[u8] = br#"{
  "schema_version": 1,
  "producer": "kd4.verify_local",
  "mode": "fast",
  "scope": {
    "scope_id": "scope-\ud83d\ude00",
    "source": "explicit",
    "active_files": [
      "src/caf\u00e9.rs"
    ],
    "owned_packages": [
      "pkg"
    ],
    "ignored_dirty_files": [],
    "adjacent_packages": [],
    "stale_reasons": [
      "line\nfeed"
    ],
    "dirty_groups": {
      "first": [
        "a"
      ],
      "second": []
    },
    "surface_rules": [
      "rule\tone"
    ]
  },
  "planned": [
    {
      "id": "cmd",
      "kind": "test",
      "command": [
        "tool",
        "--flag"
      ],
      "reason": "why",
      "owner_packages": [
        "pkg"
      ]
    }
  ],
  "skipped": [
    {
      "item": "doc",
      "reason": "skip"
    }
  ],
  "results": [
    {
      "id": "cmd",
      "command": [
        "tool",
        "--flag"
      ],
      "status": "VERIFIED",
      "exit_code": 0,
      "duration": 1.25,
      "log_path": null,
      "summary": "ok\b",
      "timed_out": false,
      "cached": true,
      "flaky": false,
      "baseline": null
    }
  ],
  "cached": [
    {
      "id": "cmd",
      "command": [
        "tool",
        "--flag"
      ],
      "status": "VERIFIED",
      "exit_code": 0,
      "duration": 1.25,
      "log_path": null,
      "summary": "ok\b",
      "timed_out": false,
      "cached": true,
      "flaky": false,
      "baseline": null
    }
  ],
  "quarantined_failures": [],
  "rerun": null,
  "cache_miss_reasons": [
    "cold"
  ],
  "verdict": "VERIFIED"
}
"#;

#[test]
fn legacy_empty_plan_matches_frozen_python_bytes() {
    let finalized = finalize_plan(
        PlanEnvelopeV2::new(PlanMode::Plan, "ignored-by-v1"),
        Vec::new(),
    );
    assert_eq!(
        serialize_legacy_v1(&finalized, false).expect("serialize"),
        FROZEN_EMPTY_PLAN_LF
    );
    let expected_crlf = String::from_utf8(FROZEN_EMPTY_PLAN_LF.to_vec())
        .expect("fixture utf8")
        .replace('\n', "\r\n")
        .into_bytes();
    assert_eq!(
        serialize_legacy_v1(&finalized, true).expect("serialize"),
        expected_crlf
    );
}

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
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            assert!(index > 0 && bytes[index - 1] == b'\r');
        }
    }
}

#[test]
fn v2_raw_path_is_lossless() {
    let raw = RawPath::new(vec![0xff, b'a']);
    let encoded = serde_json::to_value(&raw).expect("serialize");
    assert!(encoded["utf8"].is_null());
    let decoded: RawPath = serde_json::from_value(encoded).expect("deserialize");
    assert_eq!(decoded.as_bytes(), &[0xff, b'a']);
}

#[test]
fn legacy_error_has_frozen_key_order_and_exact_newline() {
    assert_eq!(
        serialize_legacy_error(Verdict::ToolingError, "bad café", false),
        b"{\n  \"schema_version\": 1,\n  \"producer\": \"kd4.verify_local\",\n  \"verdict\": \"TOOLING_ERROR\",\n  \"error\": \"bad caf\\u00e9\"\n}\n"
    );
}

#[test]
fn every_verdict_uses_the_frozen_spelling_and_exit_mapping() {
    for verdict in [
        Verdict::Planned,
        Verdict::Verified,
        Verdict::VerifiedNoProof,
        Verdict::Failed,
        Verdict::Inconclusive,
        Verdict::NeedsScope,
        Verdict::ToolingError,
        Verdict::NeedsRegen,
    ] {
        let finalized = FinalizedVerification {
            plan: PlanEnvelopeV2::new(PlanMode::Plan, "invocation"),
            results: Vec::new(),
            verdict,
            exit_code: verdict.exit_code(),
            cache_eligible: false,
            finalization_error: None,
        };
        let bytes = serialize_legacy_v1(&finalized, false).expect("serialize");
        let mut expected = FROZEN_EMPTY_PLAN_LF.to_vec();
        let marker = b"\"verdict\": \"PLANNED\"";
        let start = expected
            .windows(marker.len())
            .position(|window| window == marker)
            .expect("frozen verdict marker");
        expected.splice(
            start..start + marker.len(),
            format!("\"verdict\": \"{}\"", verdict.as_str()).bytes(),
        );
        assert_eq!(bytes, expected, "verdict {verdict:?}");
    }
}

#[test]
fn populated_legacy_payload_matches_frozen_python_bytes() {
    let mut plan = PlanEnvelopeV2::new(PlanMode::Fast, "invocation");
    plan.scope = Some(ScopeV2 {
        scope_id: "scope-😀".to_string(),
        source: "explicit".to_string(),
        active_files: vec![RawPath::from_utf8("src/café.rs")],
        owned_packages: vec!["pkg".to_string()],
        ignored_dirty_files: Vec::new(),
        adjacent_packages: Vec::new(),
        stale_reasons: vec!["line\nfeed".to_string()],
        dirty_groups: vec![
            DirtyGroup {
                id: "first".to_string(),
                paths: vec![RawPath::from_utf8("a")],
            },
            DirtyGroup {
                id: "second".to_string(),
                paths: Vec::new(),
            },
        ],
        surface_rules: vec!["rule\tone".to_string()],
    });
    plan.commands.push(CommandSpecV2 {
        id: "cmd".to_string(),
        kind: "test".to_string(),
        args: vec![CommandArgV2::text("tool"), CommandArgV2::text("--flag")],
        cwd: RawPath::from_utf8("."),
        timeout_ms: 1,
        owner_packages: vec!["pkg".to_string()],
        hash_paths: Vec::new(),
        reason: "why".to_string(),
    });
    plan.skipped.push(SkippedDecision {
        item: "doc".to_string(),
        reason: "skip".to_string(),
    });
    plan.cache_miss_reasons.push("cold".to_string());
    let finalized = finalize_plan(
        plan,
        vec![CommandResultV2 {
            schema_version: 2,
            invocation_id: "invocation".to_string(),
            command_id: "cmd".to_string(),
            command_ordinal: 0,
            runner_nonce: "nonce".to_string(),
            exit_code: Some(0),
            signal: None,
            duration_ns: 1_250_000_000,
            timed_out: false,
            cancelled: false,
            runner_error: None,
            launch_error: None,
            log_state: LogState::Complete,
            log_path: None,
            diagnostic: "ok\u{8}".to_string(),
            cached: true,
            flaky: false,
            baseline: None,
        }],
    );

    assert_eq!(
        serialize_legacy_v1(&finalized, false).expect("LF serialization"),
        FROZEN_POPULATED_LF
    );
    let expected_crlf = String::from_utf8(FROZEN_POPULATED_LF.to_vec())
        .expect("fixture utf8")
        .replace('\n', "\r\n")
        .into_bytes();
    assert_eq!(
        serialize_legacy_v1(&finalized, true).expect("CRLF serialization"),
        expected_crlf
    );
}

#[test]
fn optional_result_fields_are_null_and_command_arguments_are_preserved() {
    let mut plan = PlanEnvelopeV2::new(PlanMode::Fast, "invocation");
    plan.commands.push(CommandSpecV2 {
        id: "command".to_string(),
        kind: "test".to_string(),
        args: vec![CommandArgV2::text("tool"), CommandArgV2::text("--flag")],
        cwd: RawPath::from_utf8("."),
        timeout_ms: 1,
        owner_packages: Vec::new(),
        hash_paths: Vec::new(),
        reason: "reason".to_string(),
    });
    let finalized = finalize_plan(
        plan,
        vec![CommandResultV2 {
            schema_version: 2,
            invocation_id: "invocation".to_string(),
            command_id: "command".to_string(),
            command_ordinal: 0,
            runner_nonce: "nonce".to_string(),
            exit_code: Some(0),
            signal: None,
            duration_ns: 1_250_000_000,
            timed_out: false,
            cancelled: false,
            runner_error: None,
            launch_error: None,
            log_state: LogState::Complete,
            log_path: None,
            diagnostic: String::new(),
            cached: false,
            flaky: false,
            baseline: None,
        }],
    );
    let text = String::from_utf8(serialize_legacy_v1(&finalized, false).expect("serialize"))
        .expect("utf8");
    assert!(text.contains("\"command\": [\n        \"tool\",\n        \"--flag\"\n      ]"));
    assert!(text.contains("\"duration\": 1.25"));
    assert!(text.contains("\"log_path\": null"));
    assert!(text.contains("\"baseline\": null"));
}

#[test]
fn python_number_formatting_rejects_nonfinite_values_and_normalizes_exponents() {
    assert_eq!(python_float(1.0).expect("finite"), "1.0");
    assert_eq!(python_float(1e-7).expect("finite"), "1e-07");
    assert_eq!(python_float(1e20).expect("finite"), "1e+20");
    assert!(matches!(
        python_float(f64::NAN),
        Err(JsonContractError::NonFiniteNumber)
    ));
    assert!(matches!(
        python_float(f64::INFINITY),
        Err(JsonContractError::NonFiniteNumber)
    ));
}

#[test]
fn v1_rejects_non_utf8_paths_and_v2_is_lf_only() {
    let mut plan = PlanEnvelopeV2::new(PlanMode::Plan, "invocation");
    plan.scope = Some(ScopeV2 {
        active_files: vec![RawPath::new([0xff])],
        ..ScopeV2::default()
    });
    let finalized = finalize_plan(plan.clone(), Vec::new());
    assert!(matches!(
        serialize_legacy_v1(&finalized, false),
        Err(JsonContractError::NonUtf8Path)
    ));
    let v2 = serialize_v2_plan(&plan).expect("v2");
    assert!(v2.ends_with(b"\n"));
    assert!(!v2.windows(2).any(|window| window == b"\r\n"));
}
