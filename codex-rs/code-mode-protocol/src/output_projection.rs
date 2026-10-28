use codex_protocol::ToolName;
use serde_json::Value;

/// Display-only envelope projection, never for execution or storage.
/// Only known tool envelopes qualify; arbitrary user JSON must not be rewritten.
pub fn model_visible_tool_result(tool: &ToolName, raw: &Value) -> Option<Value> {
    if tool.namespace.is_none() && tool.name == "update_plan" {
        return plan_receipt(raw);
    }
    if tool.namespace.is_none() && tool.name == "apply_patch" {
        return patch_without_repeated_summary(raw);
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
                                && result["value"]["hydrated_ranges"].as_array().is_some_and(
                                    |ranges| {
                                        ranges.iter().any(|range| {
                                            range.get("text").is_some_and(Value::is_string)
                                        })
                                    },
                                ))
                    })
                }));
    let mcp_text = !local_text_tool
        && object
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content.iter().any(|item| {
                    item["type"] == "text" && item.get("text").is_some_and(Value::is_string)
                })
            });
    let command_streams = tool.namespace.is_none()
        && matches!(tool.name.as_str(), "exec_command" | "write_stdin")
        && object.get("streams_complete") == Some(&Value::Bool(true))
        && object.get("stdout").is_some_and(Value::is_string)
        && object.get("stderr").is_some_and(Value::is_string);
    let mcp_content = (!local_text_tool)
        .then(|| object.get("structuredContent").filter(|value| !value.is_null())
            .zip(object.get("content").and_then(Value::as_array)))
        .flatten();
    // Do not clone bodies that the display recipe will immediately discard.
    // Native results and script-visible streams keep their original values.
    let mut projected = object.iter()
        .filter(|(key, _)| !(command_streams && matches!(key.as_str(), "stdout" | "stderr" | "streams_complete"))
            && !(mcp_content.is_some() && key.as_str() == "content"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    if let Some((structured, content)) = mcp_content {
        let retained = content.iter().filter_map(|item| {
            let mirror = item["type"] == "text"
                && item["text"].as_str().is_some_and(|text| {
                    serde_json::from_str::<Value>(text).is_ok_and(|value| value == *structured)
                });
            if !mirror { return Some(item.clone()); }
            if item.as_object().is_some_and(|item| item.len() == 2) { return None; }
            // Annotations, position and provider metadata are independent
            // evidence, even when the body is an exact structured JSON mirror.
            let mut reference = item.as_object()?.iter()
                .filter(|(key, _)| key.as_str() != "text")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<serde_json::Map<_, _>>();
            reference.insert("text".into(), Value::String(
                "[Exact JSON mirror omitted; see structuredContent.]".to_string(),
            ));
            Some(Value::Object(reference))
        }).collect();
        projected.insert("content".into(), Value::Array(retained));
    }
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
        // Compatibility booleans remain script-visible. Presentation names the
        // scope rather than conflating this poll with the cumulative transcript.
        if let Some(reduced) = object.get("output_reduced").and_then(Value::as_bool) {
            projected.insert(
                "current_chunk_display_complete".into(),
                Value::Bool(!reduced),
            );
            projected.remove("output_reduced");
        }
        if let Some(complete) = object.get("streams_complete").and_then(Value::as_bool) {
            projected.insert("cumulative_streams_complete".into(), Value::Bool(complete));
            projected.remove("streams_complete");
        }
        // A producer that emits this field also knows capture finalization and
        // exact aggregation, which these envelope fields cannot show. Print the
        // value a script reads; derive it only for envelopes that lack it.
        if object
            .get("raw_output_artifact_id")
            .is_some_and(Value::is_string)
            && !object
                .get("retained_artifact_complete")
                .is_some_and(Value::is_boolean)
        {
            let complete = object.get("process_exited") == Some(&Value::Bool(true))
                && object.get("session_id").is_none_or(Value::is_null)
                && object
                    .get("raw_output_artifact_error")
                    .is_none_or(Value::is_null)
                && object.get("raw_output_artifact_retention_limit_hit")
                    == Some(&Value::Bool(false));
            projected.insert("retained_artifact_complete".into(), Value::Bool(complete));
        }
        projected.remove("output_complete");
    } else if tool.namespace.is_none()
        && matches!(tool.name.as_str(), "read_file" | "read_tool_output")
    {
        if object.get("source_sha256").is_some_and(Value::is_string)
            && object.get("canonical_sha256") == object.get("source_sha256")
        {
            projected.remove("canonical_sha256");
        }
        if tool.name == "read_file"
            && object.get("complete").is_some_and(Value::is_boolean)
            && object.get("delivered_selection_complete") == object.get("complete")
        {
            projected.remove("delivered_selection_complete");
        }
        if object.get("artifact_id") == Some(&Value::Null) {
            projected.remove("artifact_id");
        }
        // Native results retain source identity for history and scripts.
        // Presentation need not repeat an exactly reconstructible file URI.
        if let (Some(path), Some(uri)) = (
            object.get("path").and_then(Value::as_str),
            object.get("canonical_uri").and_then(Value::as_str),
        ) {
            let path = path.replace('\\', "/");
            let expected = if path.starts_with('/') {
                Some(format!("file://{path}"))
            } else if path.as_bytes().get(1) == Some(&b':')
                && path.as_bytes().get(2) == Some(&b'/')
                && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            {
                Some(format!("file:///{path}"))
            } else {
                None
            };
            if expected.as_deref() == Some(uri) {
                projected.remove("canonical_uri");
            }
        }
        if object.get("complete") == Some(&Value::Bool(true))
            && object.get("selection_status").and_then(Value::as_str) == Some("complete")
        {
            projected.remove("selection_status");
        }
        // Inline-only reads have no recovery handle. Keep failure diagnostics,
        // partial retention and every live artifact's controls explicit.
        if tool.name == "read_file"
            && object.get("artifact_id").is_none_or(Value::is_null)
            && object.get("retained_bytes").and_then(Value::as_u64) == Some(0)
            && object.get("retained_artifact_complete") == Some(&Value::Bool(false))
            && object.get("snapshot_error").is_none_or(Value::is_null)
        {
            projected.remove("retained_bytes");
            projected.remove("retained_artifact_complete");
        }
        // Keep required retention and search fields even when reconstructible:
        // direct tool responses must still satisfy their advertised schema.
        if let Some(results) = projected.get_mut("results").and_then(Value::as_array_mut) {
            for result in results {
                let Some(result) = result.as_object_mut() else {
                    continue;
                };
                project_search_metadata(result);
                if let (Some(start), Some(end), Some(bytes)) = (
                    result
                        .get("canonical_range")
                        .and_then(|range| range["start"].as_u64()),
                    result
                        .get("canonical_range")
                        .and_then(|range| range["end"].as_u64()),
                    result.get("exact_bytes").and_then(Value::as_u64),
                ) && end.checked_sub(start) == Some(bytes)
                {
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
    }
    (raw_text || mcp_text || projected != *object).then_some(Value::Object(projected))
}

/// Code Mode printing may use references that are not native schema fields.
/// Keep the native result and direct response's selector-binding contract intact.
pub fn printed_tool_result(tool: &ToolName, raw: &Value) -> Option<Value> {
    let mut projected = model_visible_tool_result(tool, raw)?;
    // Only a single exact binding/result pair is unambiguous without an index
    // map. Parser diagnostics and merged/sorted extents remain explicit.
    if tool.namespace.is_none() && tool.name == "read_file"
        && let Some(results) = raw.get("results").and_then(Value::as_array)
        && let [result] = results.as_slice()
        && result["status"] == "ok"
        && result["complete"] == true
        && (result.get("text").is_some_and(Value::is_string)
            || result.get("data_base64").is_some_and(Value::is_string))
        && let Some(selector) = result.get("selector")
        && let Some(bindings) = projected.get_mut("selector_bindings").and_then(Value::as_array_mut)
        && let [binding] = bindings.as_mut_slice()
        && binding.get("syntax_error").is_none_or(|value| value == false)
        && binding.get("resolved_selectors").and_then(Value::as_array)
            .is_some_and(|ranges| ranges.as_slice() == std::slice::from_ref(selector))
        && let Some(binding) = binding.as_object_mut()
    {
        binding.remove("resolved_selectors");
        binding.insert("result_index".into(), Value::from(0));
    }
    Some(projected)
}

/// Search coordinates and hydrated evidence remain inline. Only omit aliases
/// derivable from that same result, never a recovery path for missing context.
fn project_search_metadata(result: &mut serde_json::Map<String, Value>) {
    if result.get("status").and_then(Value::as_str) != Some("ok")
        || result
            .get("selector")
            .and_then(|selector| selector["kind"].as_str())
            != Some("search")
    {
        return;
    }
    let redundant_children = result
        .get("child_selectors")
        .and_then(Value::as_array)
        .zip(
            result
                .get("value")
                .and_then(|value| value["hydrated_ranges"].as_array()),
        )
        .is_some_and(|(children, ranges)| {
            !children.is_empty()
                && children.len() == ranges.len()
                && children.iter().zip(ranges).all(|(child, range)| {
                    range.get("selector") == Some(child)
                        && (range.get("text").is_some_and(Value::is_string)
                            || range.get("data_base64").is_some_and(Value::is_string))
                })
        });
    if redundant_children {
        result.remove("child_selectors");
    }
}

/// Keep the committed delta, retry receipt and diagnostics. Only the generic
/// rendering of an authoritative delta is redundant; custom success messages
/// and partial/failing patches are not interchangeable with generic success.
fn patch_without_repeated_summary(raw: &Value) -> Option<Value> {
    let object = raw.as_object()?;
    let mut projected = object.clone();
    if raw["success"] == true
        && raw["changes_exact"] == true
        && let (Some(text), Some(changes)) = (raw["text"].as_str(), raw["changes"].as_array())
        && generic_patch_summary(text, changes)
    {
        projected.remove("text");
    }
    // Register even when nothing can be removed: fallback display must not
    // replace a partial delta or custom diagnostic with a generic success.
    Some(Value::Object(projected))
}

/// The generic summary names each file as the patch wrote it, while the delta
/// carries resolved paths. Lines must match the delta's kinds and order.
fn generic_patch_summary(text: &str, changes: &[Value]) -> bool {
    let Some(lines) = text.strip_prefix("Success. Updated the following files:\n") else {
        return false;
    };
    let lines = match lines.strip_suffix('\n') {
        Some(lines) => lines.split('\n').collect::<Vec<_>>(),
        None if lines.is_empty() => Vec::new(),
        None => return false,
    };
    lines.len() == changes.len()
        && lines.iter().zip(changes).all(|(line, change)| {
            let (prefix, Some(path)) = (
                match change["kind"].as_str() {
                    Some("add") => "A ",
                    Some("delete") => "D ",
                    Some("update") => "M ",
                    _ => return false,
                },
                change["path"].as_str(),
            ) else {
                return false;
            };
            let Some(written) = line.strip_prefix(prefix) else {
                return false;
            };
            match change.get("move_path").filter(|v| !v.is_null()) {
                None => names_resolved_path(written, path),
                Some(destination) => {
                    prefix == "M "
                        && destination.as_str().is_some_and(|destination| {
                            written.split_once(" -> ").is_some_and(|(from, to)| {
                                names_resolved_path(from, path)
                                    && names_resolved_path(to, destination)
                            })
                        })
                }
            }
        })
}

/// A written path names a resolved one when equal to it, or to its trailing
/// components, ignoring separator spelling and ASCII case (Windows paths).
fn names_resolved_path(written: &str, resolved: &str) -> bool {
    let normalize = |path: &str| path.replace('\\', "/").to_ascii_lowercase();
    let (written, resolved) = (normalize(written), normalize(resolved));
    let written = written.strip_prefix("./").unwrap_or(&written);
    !written.is_empty()
        && !written.split('/').any(|component| component == "..")
        && (resolved == written
            || resolved
                .strip_suffix(written)
                .is_some_and(|head| head.ends_with('/')))
}

/// A compact plan receipt includes the identities needed for the next update.
/// No separate task-state prompt repeats the checklist. Scripts can read the
/// complete stored plan from the returned object.
fn plan_receipt(raw: &Value) -> Option<Value> {
    let object = raw.as_object()?;
    // Only a successful plan response has this shape; errors print unchanged.
    object.get("current_plan")?;
    let projected = object
        .iter()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "message" | "effect" | "no_progress" | "revision" | "step_ids"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    let mut projected = projected;
    if let Some(revisions) = raw["lineage"].get("step_revisions") {
        projected.insert("step_revisions".into(), revisions.clone());
    }
    Some(Value::Object(projected))
}
