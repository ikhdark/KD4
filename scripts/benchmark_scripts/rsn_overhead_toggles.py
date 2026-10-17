"""Apply env-gated reasoning-overhead toggles to an ISOLATED copy of the KD4 source.

usage: python -I -B rsn_overhead_toggles.py <isolated-repo-root>

Every toggle is off unless its environment variable is set, so one binary serves
all benchmark arms:
  KD4_RSN_NESTED  eager nested-tool declarations keep each field comment's first
                  sentence (resolve_tool keeps the complete contract) and drop the
                  apply_patch result line that restates its own description.
  KD4_RSN_SCOPE   record the exhaustive-scope notice only for spawned agents or a
                  latest user request that names exhaustive scope.
  KD4_RSN_RESULT  finished-command envelopes omit true completeness flags and the
                  implied execution_state; single-body source labels omit the body
                  index and the envelope's duplicate line count; display-reduced
                  receipts omit locators restated by their recovery call.
Refuses to patch the shared checkout.
"""

import os
import sys

root = os.path.abspath(sys.argv[1])
if (
    os.path.normcase(root).startswith(os.path.normcase(r"C:\Users\kuh\Desktop"))
    or "Temp" not in root
):
    sys.exit(f"refusing to patch a non-isolated tree: {root}")
rs = os.path.join(root, "codex-rs")


def patch(rel, pairs):
    path = os.path.join(rs, rel)
    text = open(path, encoding="utf-8", newline="").read()
    for old, new in pairs:
        count = text.count(old)
        if count != 1:
            sys.exit(f"{rel}: expected one match, found {count}: {old[:80]!r}")
        text = text.replace(old, new)
    open(path, "w", encoding="utf-8", newline="").write(text)
    print("patched", rel)


# --- KD4_RSN_NESTED -----------------------------------------------------------
patch(
    "core/src/tools/spec_plan.rs",
    [
        (
            '                if definition.name == "apply_patch" {\n'
            '                    definition.description = format!("{}\\nNested apply_patch is available',
            '                let kd4_rsn_nested = std::env::var_os("KD4_RSN_NESTED").is_some();\n'
            "                if kd4_rsn_nested {\n"
            "                    for schema in [definition.input_schema.as_mut(), definition.output_schema.as_mut()]\n"
            "                        .into_iter()\n"
            "                        .flatten()\n"
            "                    {\n"
            "                        kd4_rsn_first_sentence_descriptions(schema);\n"
            "                    }\n"
            "                }\n"
            '                if definition.name == "apply_patch" && kd4_rsn_nested {\n'
            '                } else if definition.name == "apply_patch" {\n'
            '                    definition.description = format!("{}\\nNested apply_patch is available',
        ),
        (
            "    if !has_nested_apply_patch {\n"
            '        eager_nested_tool_descriptions.push("Nested apply_patch is unavailable',
            '    if std::env::var_os("KD4_RSN_NESTED").is_some() && !eager_nested_tool_descriptions.is_empty() {\n'
            '        eager_nested_tool_descriptions.push("Declaration comments keep only their first sentence; resolve_tool(name) returns a tool\'s complete contract.".to_string());\n'
            "    }\n"
            "    if !has_nested_apply_patch {\n"
            '        eager_nested_tool_descriptions.push("Nested apply_patch is unavailable',
        ),
        (
            "fn build_code_mode_executors(\n",
            "fn kd4_rsn_first_sentence_descriptions(value: &mut serde_json::Value) {\n"
            "    match value {\n"
            "        serde_json::Value::Object(map) => {\n"
            "            for (key, child) in map.iter_mut() {\n"
            '                if key == "description" {\n'
            "                    if let serde_json::Value::String(text) = child {\n"
            "                        if let Some(end) = kd4_rsn_first_sentence_end(text) {\n"
            "                            text.truncate(end);\n"
            "                        }\n"
            "                        continue;\n"
            "                    }\n"
            "                }\n"
            "                kd4_rsn_first_sentence_descriptions(child);\n"
            "            }\n"
            "        }\n"
            "        serde_json::Value::Array(items) => {\n"
            "            for item in items {\n"
            "                kd4_rsn_first_sentence_descriptions(item);\n"
            "            }\n"
            "        }\n"
            "        _ => {}\n"
            "    }\n"
            "}\n"
            "\n"
            "fn kd4_rsn_first_sentence_end(text: &str) -> Option<usize> {\n"
            "    let bytes = text.as_bytes();\n"
            "    let mut index = 0;\n"
            "    while index + 1 < bytes.len() {\n"
            "        if bytes[index] == b'.' && matches!(bytes[index + 1], b' ' | b'\\n') {\n"
            "            let prefix = &text[..=index];\n"
            '            if !["e.g.", "i.e.", "etc.", "vs."].iter().any(|abbr| prefix.ends_with(abbr)) {\n'
            "                return Some(index + 1);\n"
            "            }\n"
            "        }\n"
            "        index += 1;\n"
            "    }\n"
            "    None\n"
            "}\n"
            "\n"
            "fn build_code_mode_executors(\n",
        ),
    ],
)

# --- KD4_RSN_SCOPE ------------------------------------------------------------
patch(
    "core/src/session/turn.rs",
    [
        (
            "    record_context_notice_if_changed(\n"
            "        sess,\n"
            "        turn,\n"
            '        "exhaustive_scope_feasibility",\n'
            "        &exhaustive_scope_feasibility_notice(turn),\n"
            "    )\n"
            "    .await?;\n",
            '    if std::env::var_os("KD4_RSN_SCOPE").is_none()\n'
            "        || matches!(\n"
            "            turn.session_source,\n"
            "            SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::ThreadSpawn { .. })\n"
            "        )\n"
            "        || kd4_rsn_latest_request_is_exhaustive(sess.clone_history().await.raw_items())\n"
            "    {\n"
            "        record_context_notice_if_changed(\n"
            "            sess,\n"
            "            turn,\n"
            '            "exhaustive_scope_feasibility",\n'
            "            &exhaustive_scope_feasibility_notice(turn),\n"
            "        )\n"
            "        .await?;\n"
            "    }\n",
        ),
        (
            "async fn record_sampling_notices(\n",
            "fn kd4_rsn_latest_request_is_exhaustive(items: &[ResponseItem]) -> bool {\n"
            "    let Some(content) = items.iter().rev().find_map(|item| match item {\n"
            "        ResponseItem::Message { role, content, .. }\n"
            '            if role == "user" && !crate::event_mapping::is_contextual_user_message(item) =>\n'
            "        {\n"
            "            Some(content)\n"
            "        }\n"
            "        _ => None,\n"
            "    }) else {\n"
            "        return true;\n"
            "    };\n"
            "    let text = content\n"
            "        .iter()\n"
            "        .filter_map(|item| match item {\n"
            "            ContentItem::InputText { text } => Some(text.to_lowercase()),\n"
            "            _ => None,\n"
            "        })\n"
            "        .collect::<Vec<_>>()\n"
            '        .join("\\n");\n'
            "    const TERMS: &[&str] = &[\n"
            '        "all", "every", "everything", "entire", "entirely", "whole", "each", "complete",\n'
            '        "completely", "full", "fully", "exhaustive", "exhaustively", "comprehensive",\n'
            '        "comprehensively", "thorough", "thoroughly", "audit", "audits", "inventory",\n'
            '        "catalog", "catalogue", "throughout",\n'
            "    ];\n"
            "    text.split(|ch: char| !ch.is_alphanumeric()).any(|word| TERMS.contains(&word))\n"
            '        || ["line by line", "end to end", "end-to-end", "go through", "read through"]\n'
            "            .iter()\n"
            "            .any(|phrase| text.contains(phrase))\n"
            "}\n"
            "\n"
            "async fn record_sampling_notices(\n",
        ),
    ],
)

# --- KD4_RSN_RESULT -----------------------------------------------------------
patch(
    "code-mode-protocol/src/output_projection.rs",
    [
        (
            '        projected.remove("output_complete");\n'
            "    } else if tool.namespace.is_none()\n",
            '        projected.remove("output_complete");\n'
            '        if std::env::var_os("KD4_RSN_RESULT").is_some()\n'
            '            && projected.get("current_chunk_display_complete") == Some(&Value::Bool(true))\n'
            "        {\n"
            '            projected.remove("current_chunk_display_complete");\n'
            '            for key in ["cumulative_streams_complete", "retained_artifact_complete"] {\n'
            "                if projected.get(key) == Some(&Value::Bool(true)) {\n"
            "                    projected.remove(key);\n"
            "                }\n"
            "            }\n"
            '            if projected.get("execution_state").and_then(Value::as_str) == Some("exited")\n'
            '                && projected.get("exit_code").is_some_and(Value::is_number)\n'
            '                && projected.get("session_id").is_none_or(Value::is_null)\n'
            "            {\n"
            '                projected.remove("execution_state");\n'
            "            }\n"
            "        }\n"
            "    } else if tool.namespace.is_none()\n",
        ),
    ],
)
patch(
    "code-mode-runtime/src/runtime/output_projection.rs",
    [
        (
            "    let source = v8::String::new(scope, PROJECTOR)\n",
            '    let lean = std::env::var_os("KD4_RSN_RESULT").is_some();\n'
            "    let projector = if lean {\n"
            "        let replaced = PROJECTOR\n"
            '            .replacen("      envelope.output_lines = text_lines;\\n", "", 1)\n'
            '            .replacen("stringify({body: index + 1, ...body.source", "stringify({...(texts.length > 1 ? {body: index + 1} : {}), ...body.source", 1);\n'
            '        assert!(!replaced.contains("envelope.output_lines") && replaced.contains("texts.length > 1 ?"));\n'
            "        std::borrow::Cow::Owned(replaced)\n"
            "    } else {\n"
            "        std::borrow::Cow::Borrowed(PROJECTOR)\n"
            "    };\n"
            "    let source = v8::String::new(scope, &projector)\n",
        ),
    ],
)
patch(
    "core/src/tools/code_mode/mod.rs",
    [
        (
            '            if let Some(recovery) = state.get("recovery") {\n'
            '                receipt["recovery"] = recovery.clone();\n'
            "            }\n",
            '            if let Some(recovery) = state.get("recovery") {\n'
            '                receipt["recovery"] = recovery.clone();\n'
            "            }\n"
            '            if std::env::var_os("KD4_RSN_RESULT").is_some()\n'
            '                && receipt["artifact_id"].is_string()\n'
            '                && receipt["recovery"]["tool"] == "read_tool_output"\n'
            '                && receipt["recovery"]["arguments"]["artifact_id"] == receipt["artifact_id"]\n'
            "                && let Some(map) = receipt.as_object_mut()\n"
            "            {\n"
            '                map.remove("recovery_tool");\n'
            '                map.remove("artifact_id");\n'
            "            }\n",
        ),
    ],
)
print("all toggles applied")
