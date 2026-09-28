//! Regression tests and opt-in benchmarks against isolated real trace bundles.
use codex_rollout_trace::*;
use serde_json::{Value, json};
use std::time::Instant;
use tempfile::TempDir;

fn writer(home: &TempDir) -> anyhow::Result<TraceWriter> {
    let writer = TraceWriter::create(home.path(), "audit".into(), "rollout".into(), "root".into())?;
    writer.append(RawTraceEventPayload::ThreadStarted {
        thread_id: "root".into(),
        agent_path: "/root".into(),
        metadata_payload: None,
    })?;
    writer.append(RawTraceEventPayload::CodexTurnStarted {
        codex_turn_id: "turn".into(),
        thread_id: "root".into(),
    })?;
    Ok(writer)
}
fn request(writer: &TraceWriter, index: usize, input: &[Value]) -> anyhow::Result<()> {
    let payload =
        writer.write_json_payload(RawPayloadKind::InferenceRequest, &json!({"input":input}))?;
    writer.append(RawTraceEventPayload::InferenceStarted {
        inference_call_id: format!("inference-{index}"),
        thread_id: "root".into(),
        codex_turn_id: "turn".into(),
        model: "audit".into(),
        provider_name: "audit".into(),
        request_payload: payload,
    })?;
    Ok(())
}

#[test]
fn receipt_rewrite_probe() -> anyhow::Result<()> {
    for changed in [false, true] {
        let home = TempDir::new()?;
        let writer = writer(&home)?;
        let output = "observed source and successful build output ".repeat(1000);
        let receipt = json!({"version":1,"kind":"tool_history_artifact_pin","successful":true,
            "digest":"observed source and successful build output","artifact_id":"audit-read-1",
            "bytes":output.len(),"sha256":"fixture-digest",
            "retrieval":{"tool":"read_tool_output","instruction":"search lines bytes section json_pointer; continuation or child_selectors"}}).to_string();
        for index in 0..3 {
            let value = if index > 0 && changed {
                &receipt
            } else {
                &output
            };
            request(
                &writer,
                index,
                &[json!({"type":"function_call_output","call_id":"read-1","output":value})],
            )?;
        }
        let start = Instant::now();
        let result = replay_bundle(home.path());
        println!(
            "AUDIT receipt_rewrite changed={changed} elapsed_ms={:.3} error={:?}",
            start.elapsed().as_secs_f64() * 1000.0,
            result.as_ref().err().map(ToString::to_string)
        );
        if changed {
            let rollout = result?;
            assert_eq!(rollout.conversation_items.len(), 2);
            assert_ne!(
                rollout.inference_calls["inference-0"].request_item_ids,
                rollout.inference_calls["inference-1"].request_item_ids
            );
            let original = &rollout.inference_calls["inference-0"].request_item_ids[0];
            let rewritten = &rollout.inference_calls["inference-1"].request_item_ids[0];
            assert_eq!(
                rollout.conversation_items[original].body.parts,
                vec![ConversationPart::Text { text: output }]
            );
            assert_eq!(
                rollout.conversation_items[rewritten].body.parts,
                vec![ConversationPart::Text { text: receipt }]
            );
            assert_eq!(
                rollout.inference_calls["inference-1"].request_item_ids,
                rollout.inference_calls["inference-2"].request_item_ids
            );
        } else {
            assert_eq!(result?.conversation_items.len(), 1);
        }
    }
    Ok(())
}

#[test]
fn repeated_image_probe() -> anyhow::Result<()> {
    for tool_output in [false, true] {
        let home = TempDir::new()?;
        let writer = writer(&home)?;
        // A real 1x1 PNG; equality, not image size, is the protected property.
        let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=";
        let parts = json!([{"type":"input_image","image_url":image}]);
        let item = if tool_output {
            json!({"type":"function_call_output","call_id":"image-1","output":parts})
        } else {
            json!({"type":"message","role":"user","content":parts})
        };
        for index in 0..20 {
            request(&writer, index, std::slice::from_ref(&item))?;
        }
        let start = Instant::now();
        let result = replay_bundle(home.path());
        println!(
            "AUDIT repeated_image tool_output={tool_output} elapsed_ms={:.3} items={:?} error={:?}",
            start.elapsed().as_secs_f64() * 1000.0,
            result.as_ref().ok().map(|r| r.conversation_items.len()),
            result.as_ref().err().map(ToString::to_string)
        );
        let rollout = result?;
        assert_eq!(rollout.conversation_items.len(), 1);
        assert_eq!(
            rollout.inference_calls["inference-0"].request_item_ids,
            rollout.inference_calls["inference-19"].request_item_ids
        );
    }
    Ok(())
}

#[test]
#[ignore]
fn tool_link_scaling_probe() -> anyhow::Result<()> {
    let corpus = include_str!("../src/reducer/conversation.rs");
    for registered in [false, true] {
        for count in [300, 1200] {
            let home = TempDir::new()?;
            let writer = writer(&home)?;
            let mut input = Vec::new();
            for index in 0..count {
                let call = format!("call-{index}");
                if registered {
                    writer.append_with_context(
                        RawTraceEventContext {
                            thread_id: Some("root".into()),
                            codex_turn_id: Some("turn".into()),
                        },
                        RawTraceEventPayload::ToolCallStarted {
                            tool_call_id: format!("tool-{index}"),
                            model_visible_call_id: Some(call.clone()),
                            code_mode_runtime_tool_id: None,
                            requester: RawToolCallRequester::Model,
                            kind: ToolCallKind::Other {
                                name: "read_file".into(),
                            },
                            summary: ToolCallSummary::Generic {
                                label: "read_file".into(),
                                input_preview: None,
                                output_preview: None,
                            },
                            invocation_payload: None,
                        },
                    )?;
                }
                input.push(json!({"type":"function_call","name":"read_file","call_id":call,"arguments":format!("{{\"path\":\"file-{index}\"}}") }));
                let offset = (index * 997) % (corpus.len() - 1024);
                let source = String::from_utf8_lossy(&corpus.as_bytes()[offset..offset + 1024]);
                input.push(json!({"type":"function_call_output","call_id":call,"output":format!("source {index} {source}")}));
            }
            for index in 0..20 {
                request(&writer, index, &input)?;
            }
            for run in 0..2 {
                let start = Instant::now();
                let rollout = replay_bundle(home.path())?;
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(rollout.conversation_items.len(), count * 2);
                assert_eq!(rollout.tool_calls.len(), if registered { count } else { 0 });
                assert_eq!(rollout.inference_calls.len(), 20);
                if registered {
                    for index in 0..count {
                        let tool = &rollout.tool_calls[&format!("tool-{index}")];
                        assert_eq!(tool.model_visible_call_item_ids.len(), 1);
                        assert_eq!(tool.model_visible_output_item_ids.len(), 1);
                    }
                }
                println!(
                    "AUDIT tool_link_scaling registered={registered} count={count} snapshots=20 run={run} replay_ms={elapsed:.3}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn media_identity_preserves_changes_and_duplicate_occurrences() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let writer = writer(&home)?;
    let image = |url| json!({"type":"message","role":"user","content":[{"type":"input_image","image_url":url}]});
    let first = image("https://example.invalid/first.png");
    let second = image("https://example.invalid/second.png");
    request(&writer, 0, std::slice::from_ref(&first))?;
    request(&writer, 1, &[second, first.clone(), first])?;
    let rollout = replay_bundle(home.path())?;
    let before = &rollout.inference_calls["inference-0"].request_item_ids;
    let after = &rollout.inference_calls["inference-1"].request_item_ids;
    assert_eq!(after.len(), 3);
    assert_ne!(after[0], before[0]);
    assert_eq!(after[1], before[0]);
    assert_ne!(after[1], after[2]);
    Ok(())
}

#[test]
fn conflicting_outputs_in_one_snapshot_are_rejected() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let writer = writer(&home)?;
    request(
        &writer,
        0,
        &[
            json!({"type":"function_call_output","call_id":"a","output":"first"}),
            json!({"type":"function_call_output","call_id":"a","output":"conflicting"}),
        ],
    )?;
    assert!(
        replay_bundle(home.path())
            .unwrap_err()
            .to_string()
            .contains("reused with different content")
    );
    Ok(())
}
