use super::*;
use std::hint::black_box;
use std::time::Instant;

fn call(index: usize) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        call_id: format!("call-{index}"),
        name: "read_file".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn image(index: usize) -> ResponseItem {
    ResponseItem::ImageGenerationCall {
        id: Some(ResponseItemId::with_suffix("ig", index.to_string())),
        status: "completed".to_string(),
        revised_prompt: None,
        result: "Zm9v".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn assembly_audit_merge_matches_reverse_insertion_for_sparse_and_dense_history() {
    for count in 0..=7 {
        let original: Vec<_> = (0..count).map(call).collect();
        for mask in 0..(1usize << count) {
            let insertions: Vec<_> = (0..count)
                .filter(|index| mask & (1 << index) != 0)
                .map(|index| (index, image(index)))
                .collect();
            let mut expected = original.clone();
            for (index, item) in insertions.iter().rev() {
                expected.insert(index + 1, item.clone());
            }
            let mut actual = original.clone();
            insert_items_after(&mut actual, insertions);
            assert_eq!(actual, expected, "count={count}, mask={mask}");
            assert_eq!(serde_json::to_vec(&actual).unwrap(), serde_json::to_vec(&expected).unwrap());
        }
    }
}

#[test]
fn assembly_audit_empty_and_single_insertions_reuse_available_storage() {
    let mut items = Vec::with_capacity(8);
    items.push(call(0));
    let storage = items.as_ptr();
    insert_items_after(&mut items, Vec::new());
    assert_eq!(items.as_ptr(), storage);
    insert_items_after(&mut items, vec![(0, image(0))]);
    assert_eq!(items.as_ptr(), storage);
    assert_eq!(items, vec![call(0), image(0)]);
}

#[test]
fn assembly_audit_missing_outputs_preserve_order_and_are_idempotent() {
    let mut items = vec![call(0), call(1), call(2)];
    ensure_call_outputs_present(&mut items);
    assert_eq!(items.len(), 6);
    for (index, pair) in items.chunks_exact(2).enumerate() {
        assert_eq!(pair[0], call(index));
        assert!(matches!(&pair[1], ResponseItem::FunctionCallOutput { call_id, output, .. }
            if call_id == &format!("call-{index}") && output == &FunctionCallOutputPayload::from_text(MISSING_TOOL_RESULT.to_string())));
    }
    let expected = items.clone();
    ensure_call_outputs_present(&mut items);
    remove_orphan_outputs(&mut items);
    assert_eq!(items, expected);
}

#[test]
fn assembly_audit_image_receipts_preserve_order_and_are_idempotent() {
    let mut items = vec![image(0), ResponseItem::Other, image(1)];
    strip_images_when_unsupported(&[InputModality::Text], &mut items);
    assert_eq!(items.len(), 5);
    assert_eq!(items[2], ResponseItem::Other);
    for index in [0, 3] {
        assert!(matches!(&items[index], ResponseItem::ImageGenerationCall { result, .. } if result.is_empty()));
        assert!(matches!(&items[index + 1], ResponseItem::Message { role, content, .. }
            if role == "developer" && content == &vec![ContentItem::InputText {
                text: format!("Generated {IMAGE_CONTENT_OMITTED_PLACEHOLDER}"),
            }]));
    }
    let expected = items.clone();
    strip_images_when_unsupported(&[InputModality::Text], &mut items);
    assert_eq!(items, expected);
}

/// Local assembly costs only: excludes fixture cloning, transport, and model latency.
/// Run explicitly with --run-ignored only. Set CODEX_CONTEXT_ASSEMBLY_BENCH_OUTPUT
/// to retain measurements when the runner suppresses successful test output.
#[test]
#[ignore = "narrow context assembly microbenchmark"]
fn assembly_audit_benchmark() {
    let mut report = String::new();
    for count in [128, 2048, 8192] {
        let missing: Vec<_> = (0..count).map(call).collect();
        let images: Vec<_> = (0..count).map(image).collect();
        let mut complete = missing.clone();
        ensure_call_outputs_present(&mut complete);
        for (name, fixture, operation) in [
            ("missing_outputs", &missing, ensure_call_outputs_present as fn(&mut Vec<ResponseItem>)),
            ("image_omissions", &images, strip_text_only as fn(&mut Vec<ResponseItem>)),
            ("no_orphan_outputs", &complete, remove_orphan_outputs as fn(&mut Vec<ResponseItem>)),
        ] {
            let mut samples = Vec::new();
            for _ in 0..11 {
                let mut items = fixture.clone();
                let started = Instant::now();
                operation(black_box(&mut items));
                samples.push(started.elapsed().as_nanos());
                black_box(items);
            }
            samples.sort_unstable();
            let row = format!("assembly_audit {name} count={count} median_ns={} min_ns={} max_ns={}\n", samples[5], samples[0], samples[10]);
            eprint!("{row}");
            report.push_str(&row);
        }
    }
    if let Some(path) = std::env::var_os("CODEX_CONTEXT_ASSEMBLY_BENCH_OUTPUT") {
        let path = std::path::PathBuf::from(path);
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, report)
            .unwrap_or_else(|error| panic!("benchmark report {}: {error}", path.display()));
    }
}

fn strip_text_only(items: &mut Vec<ResponseItem>) {
    strip_images_when_unsupported(&[InputModality::Text], items);
}
