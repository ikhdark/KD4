use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use std::fs;
use std::io;
use std::path::Path;

pub(super) fn read_events_for_remote_plugin(
    path: &Path,
    remote_plugin_id: &str,
) -> Result<Vec<Value>> {
    Ok(read_capture_events(path)?
        .into_iter()
        .filter(|event| event["event_params"]["remote_plugin_id"] == remote_plugin_id)
        .collect())
}

pub(super) fn read_capture_events(path: &Path) -> Result<Vec<Value>> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("read capture file {}", path.display()));
        }
    };
    let mut captured = Vec::new();
    let mut lines = contents.split(|byte| *byte == b'\n').peekable();
    let mut index = 0;
    while let Some(line) = lines.next() {
        index += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut payload: Value = match serde_json::from_slice(line) {
            Ok(payload) => payload,
            Err(err) if lines.peek().is_none() && err.is_eof() => break,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "parse analytics capture line {index} from {}",
                        path.display()
                    )
                });
            }
        };
        let events = payload
            .get_mut("events")
            .and_then(Value::as_array_mut)
            .context("analytics capture payload is missing events")?;
        captured.append(events);
    }
    Ok(captured)
}

pub(super) struct PluginEventIdentity<'a> {
    pub(super) plugin_id: &'a str,
    pub(super) remote_plugin_id: &'a str,
    pub(super) plugin_name: &'a str,
    pub(super) marketplace_name: &'a str,
}

pub(super) fn validate_mutation_events(
    events: Vec<Value>,
    expected: PluginEventIdentity<'_>,
) -> Result<Vec<Value>> {
    let mut validated = Vec::new();
    for event_type in ["codex_plugin_installed", "codex_plugin_uninstalled"] {
        let matching = events
            .iter()
            .filter(|event| event["event_type"] == event_type)
            .collect::<Vec<_>>();
        let [event] = matching.as_slice() else {
            bail!(
                "expected exactly one `{event_type}` event for `{}`, found {}",
                expected.remote_plugin_id,
                matching.len()
            );
        };
        validate_event(event, &expected)?;
        validated.push((*event).clone());
    }
    Ok(validated)
}

fn validate_event(event: &Value, expected: &PluginEventIdentity<'_>) -> Result<()> {
    let params = &event["event_params"];
    require_string(params, "plugin_id", expected.plugin_id)?;
    require_string(params, "remote_plugin_id", expected.remote_plugin_id)?;
    require_string(params, "plugin_name", expected.plugin_name)?;
    require_string(params, "marketplace_name", expected.marketplace_name)?;
    validate_capability_metadata(params)?;
    require_field_type(params, "product_client_id", Value::is_string)
}

pub(super) fn validate_capability_metadata(params: &Value) -> Result<()> {
    require_field_type(params, "has_skills", Value::is_boolean)?;
    require_field_type(params, "mcp_server_count", |value| value.as_u64().is_some())?;
    require_field_type(params, "connector_ids", is_string_array)
}

pub(super) fn is_string_array(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|values| values.iter().all(Value::is_string))
}

pub(super) fn require_field_type(
    params: &Value,
    field: &str,
    valid: impl FnOnce(&Value) -> bool,
) -> Result<()> {
    if !params.get(field).is_some_and(valid) {
        bail!("analytics event has invalid or missing `{field}`");
    }
    Ok(())
}

fn require_string(params: &Value, field: &str, expected: &str) -> Result<()> {
    let actual = params.get(field).and_then(Value::as_str);
    if actual != Some(expected) {
        bail!("expected `{field}` to be `{expected}`, got {actual:?}");
    }
    Ok(())
}

#[cfg(test)]
#[path = "plugin_analytics_capture_tests.rs"]
mod tests;
