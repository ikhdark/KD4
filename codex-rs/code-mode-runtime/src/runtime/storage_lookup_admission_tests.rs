use super::*;

#[tokio::test]
async fn late_callback_evidence_survives_error_persistence_without_replay() {
    let (tx, _termination, mut rx) = start(r#"
        try { await tools.sample_tool({}); throw Error('timely success'); }
        catch (error) {
            if (error.results?.completed.value.recovery_handle !== 'unique-late') throw error;
            if (error.results.completed.retry_safe !== false) throw Error('retry safety lost');
            store('late', error);
            text(load('late').results.completed.value);
        }
    "#).await;
    let RuntimeEvent::ToolCall { id, .. } = next(&mut rx).await else { panic!("tool call"); };
    tx.send(RuntimeCommand::ToolResponseAfterDeadline {
        id, result: json!({"recovery_handle":"unique-late"}),
        error_text: "deadline elapsed after completion; do not replay".into(),
    }).unwrap();
    assert_eq!(serde_json::from_str::<JsonValue>(&text(next(&mut rx).await)).unwrap(), json!({"recovery_handle":"unique-late"}));
    let RuntimeEvent::Result { error_text, stored_value_writes, .. } = next(&mut rx).await else { panic!("result"); };
    assert_eq!(error_text, None);
    assert_eq!(stored_value_writes["late"].value["results"]["completed"]["value"]["recovery_handle"], "unique-late");
    closed(&mut rx).await;
}

#[tokio::test]
async fn failed_reentrant_serialization_never_inserts_the_outer_value() {
    let (_tx, _termination, mut rx) = start(r#"
        store('kept', 1);
        try { store('outer', {nested: {toJSON() { deleteStored('kept'); throw Error('expected'); }}}); } catch {}
        if (load('outer') !== undefined || load('kept') !== undefined) throw Error('outer write committed');
        text('atomic');
    "#).await;
    assert_eq!(text(next(&mut rx).await), "atomic");
    let RuntimeEvent::Result { error_text, stored_value_writes, .. } = next(&mut rx).await else { panic!("result"); };
    assert_eq!(error_text, None);
    assert!(!stored_value_writes.contains_key("outer"));
    assert!(stored_value_writes["kept"].deleted);
    closed(&mut rx).await;
}

#[tokio::test]
async fn largest_admissible_stored_key_is_still_loadable() {
    let (_tx, _termination, mut rx) = start(r#"
        const key = 'k'.repeat(8388608 - 6); // JSON key quotes plus null's four bytes.
        store(key, null);
        if (load(key) !== null) throw Error('admissible key rejected');
        text('boundary');
    "#).await;
    assert_eq!(text(next(&mut rx).await), "boundary");
    let RuntimeEvent::Result { error_text, .. } = next(&mut rx).await else { panic!("result"); };
    assert_eq!(error_text, None);
    closed(&mut rx).await;
}

#[tokio::test]
async fn reentrant_serialization_rechecks_the_staged_write_limit() {
    let (_tx, _termination, mut rx) = start(r#"
        const n = listKeys().usage.entry_limit;
        for (let i = 0; i < 2 * n - 1; i++) deleteStored('d' + i);
        let refused = false;
        try {
            store('outer', {nested: {toJSON() {
                deleteStored('last');
                return 1;
            }}});
        } catch { refused = true; }
        if (!refused || load('outer') !== undefined) throw Error('outer write escaped admission');
        text('refused');
    "#).await;
    assert_eq!(text(next(&mut rx).await), "refused");
    let RuntimeEvent::Result { error_text, stored_value_writes, .. } = next(&mut rx).await else {
        panic!("result");
    };
    assert_eq!(error_text, None);
    assert_eq!(stored_value_writes.len(), 2 * MAX_SESSION_STORED_VALUES);
    assert!(stored_value_writes.values().all(|value| value.deleted));
    assert!(!stored_value_writes.contains_key("outer"));
    closed(&mut rx).await;
}

#[tokio::test]
async fn reentrant_serialization_can_overwrite_an_already_staged_key_at_the_limit() {
    let (_tx, _termination, mut rx) = start(r#"
        const n = listKeys().usage.entry_limit;
        store('outer', 0);
        for (let i = 0; i < 2 * n - 2; i++) deleteStored('d' + i);
        store('outer', {nested: {toJSON() { deleteStored('last'); return 1; }}});
        text(load('outer'));
    "#).await;
    assert_eq!(serde_json::from_str::<JsonValue>(&text(next(&mut rx).await)).unwrap(), json!({"nested":1}));
    let RuntimeEvent::Result { error_text, stored_value_writes, .. } = next(&mut rx).await else {
        panic!("result");
    };
    assert_eq!(error_text, None);
    assert_eq!(stored_value_writes.len(), 2 * MAX_SESSION_STORED_VALUES);
    assert_eq!(*stored_value_writes["outer"].value, json!({"nested":1}));
    closed(&mut rx).await;
}

#[tokio::test]
async fn oversized_lookup_names_are_rejected_before_native_copying() {
    let source = r#"
        const huge = 'x'.repeat(8388609);
        let refused = 0;
        for (const lookup of [() => load(huge), () => format_tool_result(huge, {})]) {
            try { lookup(); } catch { ++refused; }
        }
        if (refused !== 2 || load('missing') !== undefined) throw Error('lookup admission failed');
        store({toString() { return 'coerced'; }}, 42);
        if (load({toString() { return 'coerced'; }}) !== 42) throw Error('key coercion changed');
        await tools.sample_tool({});
    "#;
    let (tx, _termination, mut rx) = start(source).await;
    let RuntimeEvent::ToolCall { id, .. } = next(&mut rx).await else { panic!("tool call"); };
    assert!(inspect(&tx).await.rust_conversion_bytes < 4096);
    tx.send(RuntimeCommand::ToolResponse { id, result: json!(null) }).unwrap();
    let RuntimeEvent::Result { error_text, .. } = next(&mut rx).await else { panic!("result"); };
    assert_eq!(error_text, None);
    closed(&mut rx).await;
}
