use crate::model::CommandArgV2;
use crate::model::FinalizedCommandResult;
use crate::model::FinalizedVerification;
use crate::model::PlanEnvelopeV2;
use crate::model::RawPath;
use crate::model::ScopeV2;
use crate::model::VERIFY_LOCAL_JSON_PRODUCER;
use crate::model::VERIFY_LOCAL_V1_SCHEMA_VERSION;
use std::fmt::Write as _;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum JsonContractError {
    #[error("legacy V1 cannot represent a non-UTF-8 path")]
    NonUtf8Path,
    #[error("legacy V1 cannot represent a non-finite number")]
    NonFiniteNumber,
    #[error("failed to serialize V2 JSON: {0}")]
    V2(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq)]
enum LegacyValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<LegacyValue>),
    Object(Vec<(String, LegacyValue)>),
}

pub fn serialize_v2_plan(plan: &PlanEnvelopeV2) -> Result<Vec<u8>, JsonContractError> {
    let mut bytes = serde_json::to_vec_pretty(plan)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn serialize_v2_finalized(
    finalized: &FinalizedVerification,
) -> Result<Vec<u8>, JsonContractError> {
    let mut bytes = serde_json::to_vec_pretty(finalized)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn serialize_legacy_error(
    verdict: crate::model::Verdict,
    error: &str,
    windows_newlines: bool,
) -> Vec<u8> {
    let value = LegacyValue::Object(vec![
        number_field("schema_version", VERIFY_LOCAL_V1_SCHEMA_VERSION.to_string()),
        string_field("producer", VERIFY_LOCAL_JSON_PRODUCER),
        string_field("verdict", verdict.as_str()),
        string_field("error", error),
    ]);
    let eol = if windows_newlines { "\r\n" } else { "\n" };
    let mut output = String::new();
    write_legacy_value(&mut output, &value, 0, eol);
    output.push_str(eol);
    output.into_bytes()
}

pub fn serialize_legacy_v1(
    finalized: &FinalizedVerification,
    windows_newlines: bool,
) -> Result<Vec<u8>, JsonContractError> {
    let value = legacy_payload(finalized)?;
    let eol = if windows_newlines { "\r\n" } else { "\n" };
    let mut output = String::new();
    write_legacy_value(&mut output, &value, 0, eol);
    output.push_str(eol);
    Ok(output.into_bytes())
}

fn legacy_payload(finalized: &FinalizedVerification) -> Result<LegacyValue, JsonContractError> {
    let plan = &finalized.plan;
    let scope = plan.scope.as_ref().map(legacy_scope).transpose()?;
    let planned = plan
        .commands
        .iter()
        .map(|command| {
            let args = command
                .args
                .iter()
                .map(legacy_command_arg)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(LegacyValue::Object(vec![
                string_field("id", &command.id),
                string_field("kind", &command.kind),
                ("command".to_string(), LegacyValue::Array(args)),
                string_field("reason", &command.reason),
                (
                    "owner_packages".to_string(),
                    string_array(&command.owner_packages),
                ),
            ]))
        })
        .collect::<Result<Vec<_>, JsonContractError>>()?;
    let skipped = plan
        .skipped
        .iter()
        .map(|entry| {
            LegacyValue::Object(vec![
                string_field("item", &entry.item),
                string_field("reason", &entry.reason),
            ])
        })
        .collect();
    let results = finalized
        .results
        .iter()
        .map(|result| legacy_result(result, plan.commands.get(result.raw.command_ordinal)))
        .collect::<Result<Vec<_>, _>>()?;
    let cached = finalized
        .results
        .iter()
        .filter(|result| result.raw.cached)
        .map(|result| legacy_result(result, plan.commands.get(result.raw.command_ordinal)))
        .collect::<Result<Vec<_>, _>>()?;
    let rerun = finalized
        .results
        .iter()
        .find(|result| result.status.as_str() != "VERIFIED")
        .map(|result| {
            plan.commands
                .get(result.raw.command_ordinal)
                .map(|command| command.display_lossy())
                .unwrap_or_else(|| result.raw.command_id.clone())
        });

    Ok(LegacyValue::Object(vec![
        number_field("schema_version", VERIFY_LOCAL_V1_SCHEMA_VERSION.to_string()),
        string_field("producer", VERIFY_LOCAL_JSON_PRODUCER),
        string_field("mode", plan.mode.as_str()),
        ("scope".to_string(), scope.unwrap_or(LegacyValue::Null)),
        ("planned".to_string(), LegacyValue::Array(planned)),
        ("skipped".to_string(), LegacyValue::Array(skipped)),
        ("results".to_string(), LegacyValue::Array(results)),
        ("cached".to_string(), LegacyValue::Array(cached)),
        (
            "quarantined_failures".to_string(),
            LegacyValue::Array(Vec::new()),
        ),
        (
            "rerun".to_string(),
            rerun.map(LegacyValue::String).unwrap_or(LegacyValue::Null),
        ),
        (
            "cache_miss_reasons".to_string(),
            string_array(&plan.cache_miss_reasons),
        ),
        string_field("verdict", finalized.verdict.as_str()),
    ]))
}

fn legacy_scope(scope: &ScopeV2) -> Result<LegacyValue, JsonContractError> {
    let dirty_groups = scope
        .dirty_groups
        .iter()
        .map(|group| {
            Ok((
                group.id.clone(),
                LegacyValue::Array(
                    group
                        .paths
                        .iter()
                        .map(legacy_path)
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            ))
        })
        .collect::<Result<Vec<_>, JsonContractError>>()?;
    Ok(LegacyValue::Object(vec![
        string_field("scope_id", &scope.scope_id),
        string_field("source", &scope.source),
        (
            "active_files".to_string(),
            legacy_path_array(&scope.active_files)?,
        ),
        (
            "owned_packages".to_string(),
            string_array(&scope.owned_packages),
        ),
        (
            "ignored_dirty_files".to_string(),
            legacy_path_array(&scope.ignored_dirty_files)?,
        ),
        (
            "adjacent_packages".to_string(),
            string_array(&scope.adjacent_packages),
        ),
        (
            "stale_reasons".to_string(),
            string_array(&scope.stale_reasons),
        ),
        (
            "dirty_groups".to_string(),
            LegacyValue::Object(dirty_groups),
        ),
        (
            "surface_rules".to_string(),
            string_array(&scope.surface_rules),
        ),
    ]))
}

fn legacy_result(
    result: &FinalizedCommandResult,
    command: Option<&crate::model::CommandSpecV2>,
) -> Result<LegacyValue, JsonContractError> {
    let duration = python_float(result.raw.duration_ns as f64 / 1_000_000_000.0)?;
    let command = command
        .map(|command| {
            command
                .args
                .iter()
                .map(legacy_command_arg)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(LegacyValue::Object(vec![
        string_field("id", &result.raw.command_id),
        ("command".to_string(), LegacyValue::Array(command)),
        string_field("status", result.status.as_str()),
        (
            "exit_code".to_string(),
            result
                .raw
                .exit_code
                .map(|code| LegacyValue::Number(code.to_string()))
                .unwrap_or(LegacyValue::Null),
        ),
        ("duration".to_string(), LegacyValue::Number(duration)),
        (
            "log_path".to_string(),
            result
                .raw
                .log_path
                .as_ref()
                .map(|path| LegacyValue::String(path.to_string_lossy().into_owned()))
                .unwrap_or(LegacyValue::Null),
        ),
        string_field("summary", &result.raw.diagnostic),
        (
            "timed_out".to_string(),
            LegacyValue::Bool(result.raw.timed_out),
        ),
        ("cached".to_string(), LegacyValue::Bool(result.raw.cached)),
        ("flaky".to_string(), LegacyValue::Bool(result.raw.flaky)),
        (
            "baseline".to_string(),
            result
                .raw
                .baseline
                .as_ref()
                .map(|value| LegacyValue::String(value.clone()))
                .unwrap_or(LegacyValue::Null),
        ),
    ]))
}

pub fn render_human(finalized: &FinalizedVerification) -> String {
    let mut output = String::new();
    let Some(scope) = finalized.plan.scope.as_ref() else {
        output.push_str("No scope selected.\n");
        output.push_str(finalized.verdict.as_str());
        output.push('\n');
        return output;
    };
    output.push_str(&format!("Scope: {}\n", scope.scope_id));
    output.push_str(&format!("Source: {}\n", scope.source));
    output.push_str(&format!(
        "Scope freshness: {}\n",
        if scope.stale_reasons.is_empty() {
            "ok"
        } else {
            "stale"
        }
    ));
    for reason in &scope.stale_reasons {
        output.push_str(&format!("- {reason}\n"));
    }
    if !scope.active_files.is_empty() {
        output.push_str("Owned files:\n");
        for path in &scope.active_files {
            output.push_str(&format!("- {}\n", path.display_lossy()));
        }
    }
    if !scope.owned_packages.is_empty() {
        output.push_str("Owned packages:\n");
        for package in &scope.owned_packages {
            output.push_str(&format!("- {package}\n"));
        }
    }
    if !finalized.plan.commands.is_empty() {
        output.push_str("Planned commands:\n");
        for command in &finalized.plan.commands {
            output.push_str(&format!("- {}: {}\n", command.id, command.display_lossy()));
        }
    }
    for skipped in &finalized.plan.skipped {
        output.push_str(&format!("Skipped {}: {}\n", skipped.item, skipped.reason));
    }
    for result in &finalized.results {
        output.push_str(&format!(
            "{}: {}\n",
            result.raw.command_id,
            result.status.as_str()
        ));
    }
    output.push_str(finalized.verdict.as_str());
    output.push('\n');
    output
}

fn legacy_command_arg(arg: &CommandArgV2) -> Result<LegacyValue, JsonContractError> {
    arg.legacy_text()
        .map(|value| LegacyValue::String(value.to_string()))
        .ok_or(JsonContractError::NonUtf8Path)
}

fn legacy_path(path: &RawPath) -> Result<LegacyValue, JsonContractError> {
    path.as_utf8()
        .map(|value| LegacyValue::String(value.to_string()))
        .ok_or(JsonContractError::NonUtf8Path)
}

fn legacy_path_array(paths: &[RawPath]) -> Result<LegacyValue, JsonContractError> {
    Ok(LegacyValue::Array(
        paths
            .iter()
            .map(legacy_path)
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

fn string_array(values: &[String]) -> LegacyValue {
    LegacyValue::Array(
        values
            .iter()
            .map(|value| LegacyValue::String(value.clone()))
            .collect(),
    )
}

fn string_field(name: &str, value: &str) -> (String, LegacyValue) {
    (name.to_string(), LegacyValue::String(value.to_string()))
}

fn number_field(name: &str, value: String) -> (String, LegacyValue) {
    (name.to_string(), LegacyValue::Number(value))
}

fn python_float(value: f64) -> Result<String, JsonContractError> {
    if !value.is_finite() {
        return Err(JsonContractError::NonFiniteNumber);
    }
    let mut rendered = format!("{value:?}");
    if let Some(index) = rendered.find('e') {
        let exponent = rendered[index + 1..].parse::<i32>().unwrap_or_default();
        rendered.truncate(index);
        write!(&mut rendered, "e{exponent:+03}").expect("writing to String cannot fail");
    }
    Ok(rendered)
}

fn write_legacy_value(output: &mut String, value: &LegacyValue, depth: usize, eol: &str) {
    match value {
        LegacyValue::Null => output.push_str("null"),
        LegacyValue::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        LegacyValue::Number(value) => output.push_str(value),
        LegacyValue::String(value) => write_escaped_string(output, value),
        LegacyValue::Array(values) => {
            if values.is_empty() {
                output.push_str("[]");
                return;
            }
            output.push('[');
            output.push_str(eol);
            for (index, value) in values.iter().enumerate() {
                write_indent(output, depth + 1);
                write_legacy_value(output, value, depth + 1, eol);
                if index + 1 != values.len() {
                    output.push(',');
                }
                output.push_str(eol);
            }
            write_indent(output, depth);
            output.push(']');
        }
        LegacyValue::Object(fields) => {
            if fields.is_empty() {
                output.push_str("{}");
                return;
            }
            output.push('{');
            output.push_str(eol);
            for (index, (name, value)) in fields.iter().enumerate() {
                write_indent(output, depth + 1);
                write_escaped_string(output, name);
                output.push_str(": ");
                write_legacy_value(output, value, depth + 1, eol);
                if index + 1 != fields.len() {
                    output.push(',');
                }
                output.push_str(eol);
            }
            write_indent(output, depth);
            output.push('}');
        }
    }
}

fn write_indent(output: &mut String, depth: usize) {
    for _ in 0..depth * 2 {
        output.push(' ');
    }
}

fn write_escaped_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1f}' => {
                write!(output, "\\u{:04x}", character as u32)
                    .expect("writing to String cannot fail");
            }
            character if character.is_ascii() => output.push(character),
            character => {
                let codepoint = character as u32;
                if codepoint <= 0xffff {
                    write!(output, "\\u{codepoint:04x}").expect("writing to String cannot fail");
                } else {
                    let adjusted = codepoint - 0x1_0000;
                    let high = 0xd800 + (adjusted >> 10);
                    let low = 0xdc00 + (adjusted & 0x3ff);
                    write!(output, "\\u{high:04x}\\u{low:04x}")
                        .expect("writing to String cannot fail");
                }
            }
        }
    }
    output.push('"');
}

#[cfg(test)]
#[path = "json_tests.rs"]
mod tests;
