use anyhow::Result;
use anyhow::anyhow;
use codex_config::types::McpServerEnvVar;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;

pub(crate) fn create_env_for_mcp_server(
    extra_env: Option<HashMap<OsString, OsString>>,
    env_vars: &[McpServerEnvVar],
) -> Result<HashMap<OsString, OsString>> {
    let additional_env_vars = local_stdio_env_var_names(env_vars)?;
    let inherited = DEFAULT_ENV_VARS
        .iter()
        .copied()
        .chain(additional_env_vars)
        .filter_map(|var| env::var_os(var).map(|value| (OsString::from(var), value)));
    Ok(merge_env_layers(inherited, extra_env))
}

pub(crate) fn env_keys_equal(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    left.as_encoded_bytes()
        .eq_ignore_ascii_case(right.as_encoded_bytes())
}

pub(crate) fn create_env_overlay_for_remote_mcp_server(
    extra_env: Option<HashMap<OsString, OsString>>,
    env_vars: &[McpServerEnvVar],
) -> HashMap<OsString, OsString> {
    // Remote stdio should inherit PATH/HOME/etc. from the executor side, not
    // from the orchestrator process. Only forward variables explicitly named
    // by the MCP config plus literal env overrides from that config.
    let forwarded = env_vars
        .iter()
        .filter(|var| !var.is_remote_source())
        .filter_map(|var| env::var_os(var.name()).map(|value| (OsString::from(var.name()), value)));
    merge_env_layers(forwarded, extra_env)
}

fn merge_env_layers(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    extra_env: Option<HashMap<OsString, OsString>>,
) -> HashMap<OsString, OsString> {
    let mut env: HashMap<OsString, OsString> = HashMap::new();
    // Match shell_environment::apply_env_overlay without converting OsString
    // data: local stdio accepts it, while the remote launcher validates Unicode.
    // Resolve aliases within each layer before applying the explicit layer, so
    // spelling cannot override layer precedence after the maps are flattened.
    for layer in [inherited.into_iter().collect(), extra_env.unwrap_or_default()] {
        let mut entries = layer.into_iter().collect::<Vec<_>>();
        entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        for (key, value) in entries {
            env.retain(|existing, _| !env_keys_equal(existing, &key));
            env.insert(key, value);
        }
    }
    env
}

pub(crate) fn remote_mcp_env_var_names(env_vars: &[McpServerEnvVar]) -> Result<Vec<String>> {
    env_vars
        .iter()
        .filter(|var| var.is_remote_source())
        .map(|var| {
            let name = var.name();
            if name.is_empty() || name.chars().any(|ch| matches!(ch, '*' | '?' | '=' | '\0')) {
                return Err(anyhow!("remote MCP environment variable name {name:?} must be a literal nonempty name without *, ?, =, or NUL"));
            }
            Ok(name.to_string())
        })
        .collect()
}

fn local_stdio_env_var_names(env_vars: &[McpServerEnvVar]) -> Result<impl Iterator<Item = &str>> {
    if let Some(remote_var) = env_vars.iter().find(|var| var.is_remote_source()) {
        return Err(anyhow!(
            "env_vars entry `{}` uses source `remote`, which requires remote MCP stdio",
            remote_var.name()
        ));
    }
    Ok(env_vars.iter().map(McpServerEnvVar::name))
}

pub(crate) fn build_default_headers(
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();

    if let Some(static_headers) = http_headers {
        for (name, value) in static_headers {
            let header_name = match HeaderName::from_bytes(name.as_bytes()) {
                Ok(name) => name,
                Err(err) => {
                    tracing::warn!("invalid HTTP header name `{name}`: {err}");
                    continue;
                }
            };
            let mut header_value = match HeaderValue::from_str(value.as_str()) {
                Ok(value) => value,
                Err(err) => {
                    tracing::warn!("invalid HTTP header value for `{name}`: {err}");
                    continue;
                }
            };
            header_value.set_sensitive(true);
            headers.insert(header_name, header_value);
        }
    }

    if let Some(env_headers) = env_http_headers {
        for (name, env_var) in env_headers {
            if let Ok(value) = env::var(&env_var) {
                if value.trim().is_empty() {
                    continue;
                }

                let header_name = match HeaderName::from_bytes(name.as_bytes()) {
                    Ok(name) => name,
                    Err(err) => {
                        tracing::warn!("invalid HTTP header name `{name}`: {err}");
                        continue;
                    }
                };

                let mut header_value = match HeaderValue::from_str(value.as_str()) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!(
                            "invalid HTTP header value read from {env_var} for `{name}`: {err}"
                        );
                        continue;
                    }
                };
                header_value.set_sensitive(true);
                headers.insert(header_name, header_value);
            }
        }
    }

    Ok(headers)
}

pub(crate) const DEFAULT_ENV_VARS: &[&str] =
    codex_protocol::shell_environment::WINDOWS_CORE_ENV_VARS;

