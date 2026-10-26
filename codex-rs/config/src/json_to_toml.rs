use serde_json::Value as JsonValue;
use toml::Value as TomlValue;

/// Converts a JSON configuration override into the TOML value model used by config layers.
pub fn json_to_toml(value: JsonValue) -> TomlValue {
    match value {
        JsonValue::Null => TomlValue::String(String::new()),
        JsonValue::Bool(value) => TomlValue::Boolean(value),
        JsonValue::Number(value) => value
            .as_i64()
            .map(TomlValue::Integer)
            .or_else(|| value.as_f64().map(TomlValue::Float))
            .unwrap_or_else(|| TomlValue::String(value.to_string())),
        JsonValue::String(value) => TomlValue::String(value),
        JsonValue::Array(values) => {
            TomlValue::Array(values.into_iter().map(json_to_toml).collect())
        }
        JsonValue::Object(values) => TomlValue::Table(
            values
                .into_iter()
                .map(|(key, value)| (key, json_to_toml(value)))
                .collect(),
        ),
    }
}
