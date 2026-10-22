//! Shared discovery-only local backend primitives, independent of core/readiness.
use crate::ProviderKind;
use serde_json::Value;
use std::io;

pub fn ollama_host_root(base_url: &str) -> &str {
    let base = base_url.trim_end_matches('/');
    base.strip_suffix("/v1")
        .unwrap_or(base)
        .trim_end_matches('/')
}

pub fn model_list_url(kind: ProviderKind, base_url: &str) -> io::Result<String> {
    match kind {
        ProviderKind::Ollama => Ok(format!("{}/api/tags", ollama_host_root(base_url))),
        ProviderKind::LmStudio => Ok(format!("{}/models", base_url.trim_end_matches('/'))),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a local provider",
        )),
    }
}

pub fn parse_model_ids(kind: ProviderKind, value: &Value) -> io::Result<Vec<String>> {
    let (array, id) = match kind {
        ProviderKind::Ollama => ("models", "name"),
        ProviderKind::LmStudio => ("data", "id"),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a local provider",
            ));
        }
    };
    value
        .get(array)
        .and_then(Value::as_array)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("No '{array}' array in response"),
            )
        })?
        .iter()
        .map(|model| {
            model
                .get(id)
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Invalid model id in response")
                })
        })
        .collect()
}
