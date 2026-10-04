//! Model-only serialization of unmodified tool return objects. Weak keys avoid
//! retaining results after JavaScript releases them; nothing crosses the wire.
use serde_json::Value;

use super::RuntimeState;
use super::value::json_to_v8;

const PROJECTOR: &str = r#"(() => {
  const stringify = JSON.stringify;
  const keys = Object.getOwnPropertyNames;
  const descriptor = Object.getOwnPropertyDescriptor;
  const prototype = Object.getPrototypeOf;
  const projections = new WeakMap();
  const get = projections.get.bind(projections);
  const set = projections.set.bind(projections);
  let active = false;
  function same(value, original, depth = 0) {
    if (value === original) return true;
    if (depth >= 128 || value === null || original === null ||
        typeof value !== 'object' || typeof original !== 'object') return false;
    if (prototype(value) !== prototype(original)) return false;
    const names = keys(original);
    if (keys(value).length !== names.length) return false;
    for (const name of names) {
      const field = descriptor(value, name);
      const source = descriptor(original, name);
      // Never evaluate accessors a second time or replace a changed result.
      if (!field || !('value' in field) || field.enumerable !== source.enumerable ||
          !same(field.value, source.value, depth + 1)) return false;
    }
    return true;
  }
  const escapes = (text) => stringify(text).length !== text.length + 2;
  const lineCount = (text) => text.split('\n').length;
  // Registered results, including ones inside batches, keep their JSON shape
  // in a one-line envelope. Counted text bodies follow in traversal order.
  // Only tool-owned text slots qualify, never arbitrary user JSON strings.
  function rawText(projected) {
    if (projected === null || typeof projected !== 'object' || Array.isArray(projected)) {
      return undefined;
    }
    if (typeof projected.output === 'string' && !('results' in projected)) {
      const {output, ...envelope} = projected;
      if (!escapes(output)) return undefined;
      envelope.output_lines = lineCount(output);
      return {envelope, texts: [output]};
    }
    if (Array.isArray(projected.results) && !('output' in projected)) {
      const texts = [];
      const frame = (item) => {
        if (item === null || typeof item !== 'object' || typeof item.text !== 'string') {
          return item;
        }
        const {text, ...rest} = item;
        texts.push(text);
        return {...rest, text_lines: lineCount(text)};
      };
      const results = projected.results.map((result) => {
        const framed = frame(result);
        if (result?.selector?.kind === 'search' &&
            Array.isArray(result.value?.hydrated_ranges)) {
          return {...framed, value: {...result.value,
            hydrated_ranges: result.value.hydrated_ranges.map(frame)}};
        }
        return framed;
      });
      if (!texts.some(escapes)) return undefined;
      return {envelope: {...projected, results}, texts};
    }
    return undefined;
  }
  return function(value, original, projected) {
    if (arguments.length === 3) {
      set(value, {original, projected});
      active = true;
      return;
    }
    if (!active) return stringify(value);
    const texts = [];
    const rendered = stringify(value, (_key, item) => {
      const entry = item !== null && typeof item === 'object' ? get(item) : undefined;
      if (!entry || !same(item, entry.original)) return item;
      const raw = rawText(entry.projected);
      if (raw === undefined) return entry.projected;
      texts.push(...raw.texts);
      return raw.envelope;
    });
    return texts.length ? rendered + '\n' + texts.join('\n') : rendered;
  };
})()"#;

pub(super) fn prepare(scope: &mut v8::PinScope<'_, '_>) -> Result<v8::Global<v8::Function>, String> {
    let source = v8::String::new(scope, PROJECTOR)
        .ok_or_else(|| "failed to allocate output projector".to_string())?;
    let function = v8::Script::compile(scope, source, None)
        .and_then(|script| script.run(scope))
        .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
        .ok_or_else(|| "failed to install output projector".to_string())?;
    Ok(v8::Global::new(scope, function))
}

pub(super) fn install(scope: &mut v8::PinScope<'_, '_>) -> Result<(), String> {
    if scope.get_slot::<RuntimeState>().is_some_and(|state| state.output_projector.is_some()) {
        return Ok(());
    }
    let function = prepare(scope)?;
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        state.output_projector = Some(function);
    }
    Ok(())
}

pub(super) fn register(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    raw: &Value,
    projected: &Value,
) -> Result<(), String> {
    let original = json_to_v8(scope, raw)
        .ok_or_else(|| "failed to serialize projection source".to_string())?;
    let projected = json_to_v8(scope, projected)
        .ok_or_else(|| "failed to serialize output projection".to_string())?;
    let function = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.output_projector.as_ref())
        .map(|function| v8::Local::new(scope, function))
        .ok_or_else(|| "output projector unavailable".to_string())?;
    let receiver = v8::undefined(scope).into();
    function
        .call(scope, receiver, &[value, original, projected])
        .ok_or_else(|| "failed to register output projection".to_string())?;
    Ok(())
}

pub(super) fn stringify<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Option<v8::Local<'s, v8::String>> {
    let function = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.output_projector.as_ref())
        .map(|function| v8::Local::new(scope, function))?;
    let receiver = v8::undefined(scope).into();
    function
        .call(scope, receiver, &[value])
        .and_then(|value| v8::Local::<v8::String>::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::*;
    use codex_code_mode_protocol::ToolDefinition;
    use serde_json::json;

    #[tokio::test]
    async fn projection_preserves_raw_values_and_only_compacts_registered_objects() {
        for (name, raw) in [
            (
                "read_file",
                json!({"source_sha256":"rev", "canonical_sha256":"rev", "complete":true,
                "delivered_selection_complete":true,"artifact_id":null,"results":[{"text":"λ 日本語"}]}),
            ),
            (
                "mcp__test__mirror",
                json!({"structuredContent":{"x":1},"isError":true,"_meta":{"id":7},
                "content":[{"type":"text","text":"{\"x\":1}"},{"type":"text","text":"caption"}]}),
            ),
        ] {
            let projected =
                codex_code_mode_protocol::model_visible_tool_result(&ToolName::plain(name), &raw)
                    .unwrap();
            let (event_tx, mut rx) = mpsc::unbounded_channel();
            let request = ExecuteRequest {
                state_path: None,
                tool_call_id: "projection".into(),
                enabled_tools: vec![ToolDefinition {
                    name: name.into(),
                    tool_name: ToolName::plain(name),
                    kind: CodeModeToolKind::Function,
                    description: "".into(),
                    input_schema: None,
                    output_schema: None,
                    default_timeout_ms: None,
                }].into(),
                source: format!(
                    r#"
                    const r = await tools.{name}({{}});
                    store('raw', r);
                    text(r); text({{wrapped:[r]}}); console.log(r);
                    text(JSON.parse(JSON.stringify(r)));
                    const child = r.results ? r.results[0] : r.structuredContent;
                    Object.defineProperty(child, 'toJSON', {{value: () => 'CUSTOM', configurable:true}});
                    text(r);
                    delete child.toJSON;
                    r.changed = true; text(r);
                    let reads = 0;
                    Object.defineProperty(r, 'accessor', {{enumerable:true, get() {{ reads++; return 7; }} }});
                    text(r); text(reads);
                "#
                ),
                yield_time_ms: None,
                max_output_tokens: None,
                default_tool_timeout_ms: None,
            };
            let (tx, _termination) = spawn_runtime(
                HashMap::new(),
                request,
                60_000,
                event_tx,
                Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES)),
                None,
            )
            .await
            .unwrap();
            let mut printed = Vec::new();
            loop {
                let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                match event {
                    RuntimeEvent::Started => {}
                    RuntimeEvent::ToolCall { id, .. } => tx
                        .send(RuntimeCommand::ToolResponse {
                            id,
                            result: raw.clone(),
                        })
                        .unwrap(),
                    RuntimeEvent::ContentItem {
                        item: FunctionCallOutputContentItem::InputText { text },
                        ..
                    } => {
                        printed.push(serde_json::from_str::<Value>(&text).unwrap());
                    }
                    RuntimeEvent::Result {
                        error_text,
                        stored_value_writes,
                        output_loss,
                    } => {
                        assert_eq!(error_text, None);
                        assert_eq!(output_loss, None);
                        assert_eq!(*stored_value_writes["raw"].value, raw);
                        break;
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
            let mut transformed = raw.clone();
            if transformed.get("results").is_some() {
                transformed["results"][0] = json!("CUSTOM");
            } else {
                transformed["structuredContent"] = json!("CUSTOM");
            }
            let mut changed = raw.clone();
            changed["changed"] = json!(true);
            let mut accessor = changed.clone();
            accessor["accessor"] = json!(7);
            assert_eq!(
                printed,
                vec![
                    projected.clone(),
                    json!({"wrapped":[projected.clone()]}),
                    projected,
                    raw,
                    transformed,
                    changed,
                    accessor,
                    json!(1)
                ]
            );
        }
    }

    #[tokio::test]
    async fn whole_and_batched_text_results_print_raw_after_counted_envelopes() {
        let source = "fn a() {\r\n    \"λ\\path\";\r\n}\r\n".repeat(128);
        let lines = source.split('\n').count();
        let command = json!({"chunk_id":"transport", "output":source,
            "exit_code":0, "execution_state":"exited", "process_exited":true,
            "output_complete":true});
        let mut command_envelope = codex_code_mode_protocol::model_visible_tool_result(
            &ToolName::plain("exec_command"),
            &command,
        )
        .unwrap();
        command_envelope.as_object_mut().unwrap().remove("output");
        command_envelope["output_lines"] = json!(lines);
        // Recovered source keeps its CRLF bytes and quotes unescaped too.
        let recovery = json!({"artifact_id":"retained", "complete":true,
            "results":[{"status":"ok", "text":"fn a() {\r\n    \"x\"\r\n}"}]});
        let recovery_envelope = json!({"artifact_id":"retained", "complete":true,
            "results":[{"status":"ok", "text_lines":3}]});
        let search = json!({"artifact_id":"search-evidence", "complete":false,
            "results":[{"selector":{"kind":"search","query":"fn a"}, "status":"ok",
                "continuation":{"kind":"search","query":"fn a","start_byte":source.len()},
                "value":{"coverage_complete":false, "remaining_match_count":1,
                    "hydrated_ranges":[{"canonical_range":{"start":0,"end":source.len()}, "text":source},
                        {"canonical_range":{"start":0,"end":source.len()},"shared":true},
                        {"canonical_range":{"start":source.len(),"end":source.len()+1},"data_base64":"/w=="}]}}]});
        let mut search_envelope = search.clone();
        let hydrated = &mut search_envelope["results"][0]["value"]["hydrated_ranges"][0];
        hydrated.as_object_mut().unwrap().remove("text");
        hydrated["text_lines"] = json!(lines);
        for (fixture, name, raw, envelope, body) in [
            (
                "command",
                "exec_command",
                command,
                command_envelope,
                source.as_str(),
            ),
            (
                "recovery",
                "read_tool_output",
                recovery,
                recovery_envelope,
                "fn a() {\r\n    \"x\"\r\n}",
            ),
            ("search", "read_tool_output", search, search_envelope, source.as_str()),
        ] {
            let (event_tx, mut rx) = mpsc::unbounded_channel();
            let request = ExecuteRequest {
                state_path: None,
                tool_call_id: "raw-projection".into(),
                enabled_tools: vec![ToolDefinition {
                    name: name.into(),
                    tool_name: ToolName::plain(name),
                    kind: CodeModeToolKind::Function,
                    description: "".into(),
                    input_schema: None,
                    output_schema: None,
                    default_timeout_ms: None,
                }]
                .into(),
                source: format!(
                    "const r = await tools.{name}({{}}); text(r); text({{part:7, results:[r,r]}}); store('raw', r); r.extra = 1; text(r);"
                ),
                yield_time_ms: None,
                max_output_tokens: None,
                default_tool_timeout_ms: None,
            };
            let (tx, _termination) = spawn_runtime(
                HashMap::new(),
                request,
                60_000,
                event_tx,
                Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES)),
                None,
            )
            .await
            .unwrap();
            let mut printed = Vec::new();
            loop {
                let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                match event {
                    RuntimeEvent::Started => {}
                    RuntimeEvent::ToolCall { id, .. } => tx
                        .send(RuntimeCommand::ToolResponse {
                            id,
                            result: raw.clone(),
                        })
                        .unwrap(),
                    RuntimeEvent::ContentItem {
                        item: FunctionCallOutputContentItem::InputText { text },
                        ..
                    } => printed.push(text),
                    RuntimeEvent::Result { error_text, stored_value_writes, output_loss } => {
                        assert_eq!(error_text, None);
                        assert_eq!(output_loss, None);
                        assert_eq!(*stored_value_writes["raw"].value, raw);
                        break;
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
            assert_eq!(printed.len(), 3, "{name}");
            let (header, printed_body) = printed[0].split_once('\n').unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(header).unwrap(),
                envelope,
                "{name}"
            );
            assert_eq!(printed_body, body, "{name}");
            let (header, printed_body) = printed[1].split_once('\n').unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(header).unwrap(),
                json!({"part":7, "results":[envelope.clone(), envelope]}),
                "batch identity and result ordering must survive projection"
            );
            assert_eq!(printed_body, format!("{body}\n{body}"), "{name}");
            let projected = codex_code_mode_protocol::model_visible_tool_result(&ToolName::plain(name), &raw).unwrap();
            let old_batch = json!({"part":7,"results":[projected.clone(),projected]}).to_string();
            if fixture != "recovery" {
                assert!(printed[1].len() < old_batch.len());
            }
            println!("projection_audit {fixture} raw_bytes={} whole_bytes={} batch_before_bytes={} batch_after_bytes={}",
                raw.to_string().len(), printed[0].len(), old_batch.len(), printed[1].len());
            // A modified result is no longer the tool's own value: print it as JSON.
            let mut changed = raw;
            changed["extra"] = json!(1);
            assert_eq!(
                serde_json::from_str::<Value>(&printed[2]).unwrap(),
                changed,
                "{name}"
            );
        }
    }
}
