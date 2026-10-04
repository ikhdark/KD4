use codex_protocol::ToolName;
use serde_json::Value;

/// Display-only envelope projection, never for execution or storage.
/// Only known tool envelopes qualify; arbitrary user JSON must not be rewritten.
pub fn model_visible_tool_result(tool: &ToolName, raw: &Value) -> Option<Value> {
    if tool.namespace.is_none() && tool.name == "update_plan" {
        return plan_without_restated_lineage(raw);
    }
    let local_text_tool = tool.namespace.is_none()
        && matches!(
            tool.name.as_str(),
            "read_file" | "read_tool_output" | "exec_command" | "write_stdin"
        );
    if !local_text_tool && tool.namespace.is_none() && !tool.name.starts_with("mcp__") {
        return None;
    }
    let object = raw.as_object()?;
    // Registration is what lets a whole printed result show its source or
    // command text raw; an envelope with nothing to compact still needs it.
    let raw_text = local_text_tool
        && (object.get("output").is_some_and(Value::is_string)
            || object
                .get("results")
                .and_then(Value::as_array)
                .is_some_and(|results| {
                    results.iter().any(|result| {
                        result.get("text").is_some_and(Value::is_string)
                            || (result["selector"]["kind"] == "search"
                                && result["value"]["hydrated_ranges"]
                                    .as_array()
                                    .is_some_and(|ranges| ranges.iter().any(|range| {
                                        range.get("text").is_some_and(Value::is_string)
                                    })))
                    })
                }));
    let mut projected = object.clone();
    if tool.namespace.is_none() && matches!(tool.name.as_str(), "exec_command" | "write_stdin") {
        // Chunk IDs identify transport frames, not resumable commands or artifacts.
        projected.remove("chunk_id");
        for key in [
            "raw_output_artifact_error",
            "raw_output_artifact_retention_limit_reason",
            "output_decoding_notice",
        ] {
            if projected.get(key) == Some(&Value::Null) {
                projected.remove(key);
            }
        }
        if projected.get("raw_output_artifact_retention_limit_hit") == Some(&Value::Bool(false)) {
            projected.remove("raw_output_artifact_retention_limit_hit");
        }
        // Omit only facts implied by an explicit, matching control field.
        // Contradictions and unknown lifecycle states must remain diagnosable.
        if object.get("execution_state").and_then(Value::as_str) == Some("exited")
            && object.get("process_exited") == Some(&Value::Bool(true))
        {
            projected.remove("process_exited");
        }
        if object.get("output_complete") == Some(&Value::Bool(true))
            && object.get("output_reduced") == Some(&Value::Bool(false))
        {
            projected.remove("output_reduced");
            // Every retained byte is already in view, so the locator recovers
            // nothing; direct responses likewise name it only when reduced.
            // Scripts keep both fields, and history pins carry their own.
            projected.remove("raw_output_artifact_id");
            projected.remove("raw_output_artifact_bytes");
        }
        // These are exact script inputs, not a second copy to spend the cell's
        // display budget on. Scripts can explicitly emit selected stream data.
        if object.get("streams_complete") == Some(&Value::Bool(true))
            && object.get("stdout").is_some_and(Value::is_string)
            && object.get("stderr").is_some_and(Value::is_string)
        {
            projected.remove("stdout");
            projected.remove("stderr");
            projected.remove("streams_complete");
        }
    } else if tool.namespace.is_none()
        && matches!(tool.name.as_str(), "read_file" | "read_tool_output")
    {
        if object.get("source_sha256").is_some_and(Value::is_string)
            && object.get("canonical_sha256") == object.get("source_sha256")
        {
            projected.remove("canonical_sha256");
        }
        if object.get("complete").is_some_and(Value::is_boolean)
            && object.get("delivered_selection_complete") == object.get("complete")
        {
            projected.remove("delivered_selection_complete");
        }
        if object.get("artifact_id") == Some(&Value::Null) {
            projected.remove("artifact_id");
        }
        if object.get("retained_artifact_complete") == Some(&Value::Bool(true))
            && object.get("canonical_bytes").is_some_and(Value::is_u64)
            && object.get("retained_bytes") == object.get("canonical_bytes")
        {
            projected.remove("retained_bytes");
        }
        if let Some(results) = projected.get_mut("results").and_then(Value::as_array_mut) {
            for result in results {
                let Some(result) = result.as_object_mut() else { continue };
                if let (Some(start), Some(end), Some(bytes)) = (
                    result.get("canonical_range").and_then(|range| range["start"].as_u64()),
                    result.get("canonical_range").and_then(|range| range["end"].as_u64()),
                    result.get("exact_bytes").and_then(Value::as_u64),
                ) && end.checked_sub(start) == Some(bytes) {
                    result.remove("exact_bytes");
                }
                // A completed drain no longer needs a traversal plan. Keep its
                // diagnostic message, selector and exact range for provenance.
                if result.get("status").and_then(Value::as_str) == Some("ok")
                    && result.get("complete") == Some(&Value::Bool(true))
                    && !result.contains_key("continuation")
                {
                    result.remove("subdivision_plan");
                }
            }
        }
    } else if tool.namespace.is_some() || tool.name.starts_with("mcp__") {
        let structured = object.get("structuredContent").filter(|v| !v.is_null())?;
        let content = object.get("content")?.as_array()?;
        let retained = content
            .iter()
            .filter(|item| {
                // Annotations and other fields are independent evidence, even on a mirror.
                !(item.as_object().is_some_and(|item| item.len() == 2)
                    && item["type"] == "text"
                    && item["text"].as_str().is_some_and(|text| {
                        serde_json::from_str::<Value>(text).is_ok_and(|value| value == *structured)
                    }))
            })
            .cloned()
            .collect::<Vec<_>>();
        if retained.len() == content.len() {
            return None;
        }
        projected.insert("content".to_string(), Value::Array(retained));
    }
    (raw_text || projected != *object).then_some(Value::Object(projected))
}

/// A requirement owned solely by the current step with its ID, text and status
/// restates `current_plan`. Renamed, merged, split, superseded and retired
/// requirements, identities and workflow links stay in view.
fn plan_without_restated_lineage(raw: &Value) -> Option<Value> {
    let object = raw.as_object()?;
    let mut lineage = object.get("lineage")?.as_object()?.clone();
    let steps = raw.pointer("/current_plan/plan")?.as_array()?;
    let step_ids = object.get("step_ids")?.as_array()?;
    if steps.len() != step_ids.len() {
        return None;
    }
    let mut requirements = lineage.get("requirements")?.as_object()?.clone();
    let mut step_requirements = lineage.get("step_requirements")?.as_object()?.clone();
    for (step, id) in steps.iter().zip(step_ids) {
        let id = id.as_str()?;
        let (text, status) = (&step["step"], &step["status"]);
        if step_requirements.get(id) == Some(&serde_json::json!([id]))
            && requirements.get(id) == Some(&serde_json::json!({"text": text, "status": status}))
        {
            step_requirements.remove(id);
            requirements.remove(id);
        }
    }
    let mut projected = object.clone();
    if requirements.is_empty() && step_requirements.is_empty() && lineage.len() == 2 {
        projected.remove("lineage");
    } else {
        lineage.insert("requirements".to_string(), Value::Object(requirements));
        lineage.insert("step_requirements".to_string(), Value::Object(step_requirements));
        projected.insert("lineage".to_string(), Value::Object(lineage));
    }
    (projected != *object).then_some(Value::Object(projected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_projection_keeps_lifecycle_and_recovery_not_transport_defaults() {
        let raw = json!({"chunk_id":"transport", "output":"progress",
            "session_id":7, "execution_state":"running", "exit_code":null,
            "raw_output_artifact_id":"retained", "output_complete":false,
            "raw_output_artifact_retention_limit_hit":false,
            "raw_output_artifact_error":null,
            "stdout":"progress", "stderr":"", "streams_complete":true});
        let compact = model_visible_tool_result(&ToolName::plain("exec_command"), &raw).unwrap();
        assert_eq!(compact, json!({"output":"progress", "session_id":7,
            "execution_state":"running", "exit_code":null,
            "raw_output_artifact_id":"retained", "output_complete":false}));
        // Execution values, including the separate streams, remain untouched.
        assert_eq!(raw["stdout"], "progress");
        let terminal = json!({"output":"done", "execution_state":"exited",
            "process_exited":true, "exit_code":0, "output_complete":true,
            "output_reduced":false, "session_id":7,
            "session_capabilities":{"polling":true},
            "raw_output_artifact_id":"retained", "raw_output_artifact_bytes":4});
        let mut expected = terminal.clone();
        for key in ["process_exited", "output_reduced", "raw_output_artifact_id",
            "raw_output_artifact_bytes"] {
            expected.as_object_mut().unwrap().remove(key);
        }
        println!("projection_audit terminal_controls before_bytes={} after_bytes={}", terminal.to_string().len(), expected.to_string().len());
        assert_eq!(model_visible_tool_result(&ToolName::plain("write_stdin"), &terminal), Some(expected));
        // A reduced result keeps the locator and selector that recover its gap.
        let reduced = json!({"output":"head\n[omitted lines 2-9 of 10]\ntail",
            "execution_state":"exited", "exit_code":0, "output_complete":false,
            "output_reduced":true, "raw_output_artifact_id":"retained",
            "raw_output_artifact_bytes":90, "recovery_selector":{"kind":"lines","start":2,"end":9}});
        assert_eq!(model_visible_tool_result(&ToolName::plain("exec_command"), &reduced), Some(reduced));
        // Exited processes may still have output to drain. Keep the handle and
        // all mismatching state, including a producer's contradictory flags.
        let inconsistent = json!({"output":"diagnostic", "execution_state":"running",
            "process_exited":true, "exit_code":null, "output_complete":false,
            "output_reduced":false, "session_id":7, "error":"cleanup failed"});
        assert_eq!(model_visible_tool_result(&ToolName::plain("exec_command"), &inconsistent), Some(inconsistent));
    }

    #[test]
    fn read_file_preserves_distinctions_and_recovery() {
        let raw = json!({"source_sha256":"revision", "canonical_sha256":"revision",
            "complete":true, "delivered_selection_complete":true, "artifact_id":null,
            "file_complete":false, "retained_artifact_complete":false, "snapshot_error":"disk full",
            "results":[{"text":"λ 日本語"}]});
        let compact = model_visible_tool_result(&ToolName::plain("read_file"), &raw).unwrap();
        assert!(compact.get("canonical_sha256").is_none());
        assert!(compact.get("delivered_selection_complete").is_none());
        assert!(compact.get("artifact_id").is_none());
        let mut restored = compact;
        for key in [
            "canonical_sha256",
            "delivered_selection_complete",
            "artifact_id",
        ] {
            restored[key] = raw[key].clone();
        }
        assert_eq!(restored, raw);
        let distinct = json!({"source_sha256":"new", "canonical_sha256":"old",
            "complete":false, "delivered_selection_complete":true, "artifact_id":"snapshot",
            "continuation":{"kind":"bytes","start":4,"end":8}});
        assert_eq!(
            model_visible_tool_result(&ToolName::plain("read_file"), &distinct),
            None
        );
        assert_eq!(
            model_visible_tool_result(&ToolName::plain("other"), &raw),
            None
        );
    }

    #[test]
    fn whole_text_results_register_without_compaction() {
        let recovery = json!({"artifact_id":"retained", "canonical_sha256":"c",
            "complete":true,
            "results":[{"status":"ok", "text":"fn a() {\r\n    \"x\"\r\n}"}]});
        let distinct_read = json!({"source_sha256":"new", "canonical_sha256":"old",
            "complete":false, "delivered_selection_complete":true, "artifact_id":"snapshot",
            "results":[{"text":"line\n"}]});
        for (name, raw) in [
            ("read_tool_output", &recovery),
            ("read_file", &distinct_read),
        ] {
            assert_eq!(
                model_visible_tool_result(&ToolName::plain(name), raw).as_ref(),
                Some(raw),
                "{name}"
            );
        }
        let search_only = json!({"artifact_id":"retained", "results":[{"value":{}}]});
        assert_eq!(
            model_visible_tool_result(&ToolName::plain("read_tool_output"), &search_only),
            None
        );
    }

    #[test]
    fn recovery_projection_preserves_evidence_and_incomplete_controls() {
        let raw = json!({"artifact_id":"retained", "canonical_sha256":"revision",
            "canonical_bytes":8, "retained_bytes":8, "retained_artifact_complete":true,
            "complete":true, "delivered_selection_complete":true,
            "results":[{"selector":{"kind":"bytes","start":2,"end":8},
                "status":"ok", "complete":true, "exact_bytes":6,
                "canonical_range":{"start":2,"end":8}, "text":"λ\r\nhi",
                "subdivision_plan":{"chunk_count":2}, "message":"drained 2 pages"}]});
        let mut expected = raw.clone();
        for field in ["retained_bytes", "delivered_selection_complete"] {
            expected.as_object_mut().unwrap().remove(field);
        }
        for field in ["exact_bytes", "subdivision_plan"] {
            expected["results"][0].as_object_mut().unwrap().remove(field);
        }
        println!("projection_audit recovery_metadata before_bytes={} after_bytes={}", raw.to_string().len(), expected.to_string().len());
        for tool in ["read_file", "read_tool_output"] {
            assert_eq!(model_visible_tool_result(&ToolName::plain(tool), &raw), Some(expected.clone()));
            let mut incomplete = raw.clone();
            incomplete["retained_artifact_complete"] = json!(false);
            incomplete["retained_bytes"] = json!(6);
            incomplete["delivered_selection_complete"] = json!(false);
            incomplete["results"][0]["exact_bytes"] = json!(9);
            incomplete["results"][0]["continuation"] = json!({"kind":"bytes","start":6,"end":8});
            assert_eq!(model_visible_tool_result(&ToolName::plain(tool), &incomplete), Some(incomplete));
        }
        assert_eq!(raw["results"][0]["text"], "λ\r\nhi");
        assert_eq!(raw["results"][0]["subdivision_plan"]["chunk_count"], 2);
    }

    #[test]
    fn plan_display_omits_only_lineage_restating_current_steps() {
        let restated = json!({"completion_authority":"checklist_only", "effect":"status_only",
            "message":"Plan updated", "no_progress":false, "revision":"rev", "step_ids":["a", "b"],
            "current_plan":{"explanation":null, "plan":[{"step":"Inspect", "status":"completed"},
                {"step":"Patch the owner", "status":"in_progress"}]},
            "lineage":{"requirements":{"a":{"text":"Inspect", "status":"completed"},
                "b":{"text":"Patch the owner", "status":"in_progress"}},
                "step_requirements":{"a":["a"], "b":["b"]}}});
        let compact = model_visible_tool_result(&ToolName::plain("update_plan"), &restated).unwrap();
        let mut expected = restated.clone();
        expected.as_object_mut().unwrap().remove("lineage");
        assert_eq!(compact, expected);
        // Every omitted entry is rebuilt from the step that owns it.
        let (mut requirements, mut step_requirements) = (json!({}), json!({}));
        for (id, step) in compact["step_ids"].as_array().unwrap().iter()
            .zip(compact["current_plan"]["plan"].as_array().unwrap())
        {
            let id = id.as_str().unwrap();
            requirements[id] = json!({"text": step["step"], "status": step["status"]});
            step_requirements[id] = json!([id]);
        }
        assert_eq!(json!({"requirements": requirements, "step_requirements": step_requirements}),
            restated["lineage"]);

        // Original wording, identities and retired obligations stay in view.
        let mut revised = restated.clone();
        revised["current_plan"]["plan"][1]["step"] = json!("Patch and test the owner");
        revised["lineage"]["step_identities"] = json!({"c":"b"});
        revised["lineage"]["requirements"]["d"] =
            json!({"text":"Drop cache", "status":"pending", "superseded_reason":"not needed"});
        let mut expected = revised.clone();
        expected["lineage"]["requirements"].as_object_mut().unwrap().remove("a");
        expected["lineage"]["step_requirements"].as_object_mut().unwrap().remove("a");
        assert_eq!(model_visible_tool_result(&ToolName::plain("update_plan"), &revised), Some(expected));
        let mut bare = restated;
        bare.as_object_mut().unwrap().remove("lineage");
        assert_eq!(model_visible_tool_result(&ToolName::plain("update_plan"), &bare), None);
    }

    #[test]
    fn mcp_removes_only_unannotated_exact_mirrors() {
        let raw = json!({"structuredContent":{"a":1,"b":2},"isError":true,"_meta":{"id":7},
            "content":[{"type":"text","text":"{\"b\":2, \"a\":1}"},
                {"type":"text","text":"caption"}, {"type":"text","text":"{\"a\":1}"},
                {"type":"image","data":"abc","mimeType":"image/png"},
                {"type":"text","text":"{\"a\":1,\"b\":2}","annotations":{"priority":1}}]});
        let mut expected = raw.clone();
        expected["content"].as_array_mut().unwrap().remove(0);
        assert_eq!(
            model_visible_tool_result(&ToolName::namespaced("server", "tool"), &raw),
            Some(expected)
        );
        assert_eq!(
            model_visible_tool_result(&ToolName::plain("other"), &raw),
            None
        );
        for structured in [Value::Null, json!({"a":2}), json!(false)] {
            let mut mismatch = raw.clone();
            mismatch["structuredContent"] = structured;
            assert_eq!(
                model_visible_tool_result(&ToolName::plain("mcp__server__tool"), &mismatch),
                None
            );
        }
    }
}
