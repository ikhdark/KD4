use codex_protocol::ToolName;
use serde_json::Value;

/// Display-only envelope projection, never for execution or storage.
/// Only known tool envelopes qualify; arbitrary user JSON must not be rewritten.
pub fn model_visible_tool_result(tool: &ToolName, raw: &Value) -> Option<Value> {
    if tool.namespace.is_none()
        && !matches!(tool.name.as_str(), "read_file" | "exec_command" | "write_stdin")
        && !tool.name.starts_with("mcp__")
    {
        return None;
    }
    let object = raw.as_object()?;
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
    } else if tool.namespace.is_none() && tool.name == "read_file" {
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
