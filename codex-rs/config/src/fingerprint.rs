use crate::schema::canonicalize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use toml::Value as TomlValue;

/// Lossless dotted map-key path. Non-bare keys are JSON quoted; array origins
/// use separate `[index]` components, never indistinguishable numeric keys.
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
    let json = serde_json::to_value(value).unwrap_or(JsonValue::Null);
    let canonical = canonicalize(&json);
    let serialized = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized);
    let hash = hex::encode(hasher.finalize());
    format!("sha256:{hash}")
}
