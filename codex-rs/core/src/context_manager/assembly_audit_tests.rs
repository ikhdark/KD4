use super::*;

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
    for (index, pair) in items.as_chunks::<2>().0.iter().enumerate() {
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




