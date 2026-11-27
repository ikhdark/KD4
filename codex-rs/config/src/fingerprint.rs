use crate::schema::canonicalize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use toml::Value as TomlValue;

/// Lossless dotted map-key path. Non-bare keys are JSON quoted; array origins
/// use separate `[index]` components, never indistinguishable numeric keys.
#[expect(
    clippy::expect_used,
    reason = "serializing a string to JSON cannot fail"
)]
pub fn config_origin_key(path: &[&str]) -> String {
    path.iter().map(|key| {
        if !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
            (*key).to_owned()
        } else {
            serde_json::to_string(key).expect("string serialization")
        }
    }).collect::<Vec<_>>().join(".")
}

pub(super) fn record_origins<T: Clone>(
    value: &TomlValue,
    meta: &T,
    path: &mut Vec<String>,
    origins: &mut HashMap<String, T>,
) {
    match value {
        TomlValue::Table(table) => {
            for (key, val) in table {
                path.push(config_origin_key(&[key]));
                record_origins(val, meta, path, origins);
                path.pop();
            }
        }
        TomlValue::Array(items) => {
            for (idx, item) in items.iter().enumerate() {
                path.push(format!("[{idx}]"));
                record_origins(item, meta, path, origins);
                path.pop();
            }
        }
        _ => {
            if !path.is_empty() {
                origins.insert(path.join("."), meta.clone());
            }
        }
    }
}

pub fn version_for_toml(value: &TomlValue) -> String {
    let mut json = serde_json::to_value(value).unwrap_or(JsonValue::Null);
    let mut has_non_finite = false;
    encode_non_finite_floats(value, &mut json, &mut has_non_finite);
    let domain: &[u8] = if has_non_finite {
        b"\0toml-nonfinite-v1\0"
    } else {
        b""
    };
    version_for_json_in_domain(&json, domain)
}

fn encode_non_finite_floats(toml: &TomlValue, json: &mut JsonValue, found: &mut bool) {
    match (toml, json) {
        (TomlValue::Float(value), json) if !value.is_finite() => {
            // TOML has no null, so this tag cannot appear in ordinary TOML.
            // Keep the bits to distinguish infinities and signed NaNs.
            *json = JsonValue::Array(vec![JsonValue::Null, value.to_bits().into()]);
            *found = true;
        }
        (TomlValue::Array(values), JsonValue::Array(items)) => {
            for (value, item) in values.iter().zip(items) {
                encode_non_finite_floats(value, item, found);
            }
        }
        (TomlValue::Table(table), JsonValue::Object(object)) => {
            for (key, value) in table {
                if let Some(item) = object.get_mut(key) {
                    encode_non_finite_floats(value, item, found);
                }
            }
        }
        _ => {}
    }
}

/// Canonical content identity for JSON inputs, including null and nested objects.
pub fn version_for_json(value: &JsonValue) -> String {
    version_for_json_in_domain(value, b"")
}

fn version_for_json_in_domain(value: &JsonValue, domain: &[u8]) -> String {
    let canonical = canonicalize(value);
    let serialized = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(serialized);
    let hash = hex::encode(hasher.finalize());
    format!("sha256:{hash}")
}
