use super::*;
use std::hint::black_box;
use std::time::Instant;

fn message(text: &str, trusted: bool) -> ResponseItem {
    let mut item = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    if trusted {
        mark_trusted_stable_context_item(&mut item);
    }
    item
}

#[test]
fn ordinary_and_empty_injections_skip_history_classification() {
    let history = vec![message(
        "<collaboration_mode>unchanged</collaboration_mode>",
        true,
    )];
    for candidates in [Vec::new(), vec![message("ordinary context", false)]] {
        CLASSIFY_STABLE_TEXT_CALLS.with(|calls| calls.set(0));
        let filtered = filter_unchanged_stable_context_items(&history, candidates.clone());
        assert_eq!(filtered, candidates);
        assert_eq!(CLASSIFY_STABLE_TEXT_CALLS.with(Cell::get), 0);
    }
}

#[test]
fn mixed_injections_still_filter_only_trusted_duplicates() {
    let trusted = message("<collaboration_mode>unchanged</collaboration_mode>", true);
    let untrusted = message("<collaboration_mode>unchanged</collaboration_mode>", false);
    let filtered = filter_unchanged_stable_context_items(
        std::slice::from_ref(&trusted),
        vec![trusted.clone(), untrusted.clone()],
    );
    assert_eq!(filtered, vec![untrusted]);
}

#[test]
fn supplied_measurements_preserve_identity_and_accounting() {
    for bytes in [b"ascii".as_slice(), "測定".as_bytes(), &[0xff, 0xfe], &[]] {
        for (serialized_bytes, approx_tokens) in [(0, 0), (12345, 678)] {
            let mut expected = component_from_bytes(
                StableContextKind::DynamicHistory,
                "dynamic_history",
                bytes,
                true,
                StableContextDisposition::Unchanged,
                None,
            );
            expected.identity.serialized_bytes = serialized_bytes;
            expected.identity.approx_tokens = approx_tokens;
            let manifest = StableContextManifest::default();
            let measured = manifest.add_measured_component(
                StableContextKind::DynamicHistory,
                "dynamic_history",
                bytes,
                serialized_bytes,
                approx_tokens,
            );
            let dynamic = manifest.add_dynamic_history(bytes, serialized_bytes, approx_tokens);
            assert_eq!(measured.components.as_ref(), &[expected.clone()]);
            assert_eq!(dynamic.components.as_ref(), &[expected]);
        }
    }
}

#[test]
fn dynamic_history_preserves_canonical_order_without_rehashing() {
    let manifest = StableContextManifest::from_components(Vec::new(), true, false)
        .add_component_bytes(
            StableContextKind::RepositoryObservation,
            "observation",
            b"seen",
        )
        .add_component_bytes(StableContextKind::Repository, "repository", b"rules")
        .add_component_bytes(StableContextKind::TaskState, "task", b"active");
    let expected = manifest.add_measured_component(
        StableContextKind::DynamicHistory,
        "dynamic_history",
        b"history",
        123,
        45,
    );
    MANIFEST_FINGERPRINT_CALLS.with(|calls| calls.set(0));
    let dynamic = manifest.add_dynamic_history(b"history", 123, 45);
    assert_eq!(MANIFEST_FINGERPRINT_CALLS.with(Cell::get), 0);
    assert_eq!(dynamic, expected);
    assert_eq!(dynamic.fingerprint, manifest.fingerprint);
}

#[test]
#[ignore = "narrow local cache benchmark; run explicitly with --run-ignored only"]
fn benchmark_stable_context_cache_processing() {
    let history = vec![message("<collaboration_mode>unchanged</collaboration_mode>", true); 4096];
    let identity = "context reuse 測定 ".repeat(16384);
    let manifest = StableContextManifest::default();
    let tokens = i64::try_from(approx_token_count(&identity)).unwrap();
    let mut report = String::new();
    for case in ["empty_injection", "ordinary_injection", "measured_history"] {
        let mut samples = Vec::new();
        for _ in 0..7 {
            let started = Instant::now();
            for _ in 0..32 {
                match case {
                    "empty_injection" => {
                        black_box(filter_unchanged_stable_context_items(
                            black_box(&history),
                            Vec::new(),
                        ));
                    }
                    "ordinary_injection" => {
                        black_box(filter_unchanged_stable_context_items(
                            black_box(&history),
                            vec![message("ordinary context", false)],
                        ));
                    }
                    _ => {
                        black_box(manifest.add_dynamic_history(
                            black_box(identity.as_bytes()),
                            identity.len() as u64,
                            tokens,
                        ));
                    }
                }
            }
            samples.push(started.elapsed().as_nanos() / 32);
        }
        samples.sort_unstable();
        report.push_str(&format!("cache_benchmark case={case} iterations=224 median_ns={} min_ns={} max_ns={} history_items={} identity_bytes={}\n",
            samples[3], samples[0], samples[6], history.len(), identity.len()));
    }
    eprint!("{report}");
    if let Some(path) = std::env::var_os("CODEX_CACHE_BENCHMARK_OUTPUT") {
        std::fs::write(path, report).unwrap();
    }
}
