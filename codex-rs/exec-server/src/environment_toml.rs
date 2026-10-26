use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::DefaultEnvironmentProvider;
use crate::Environment;
use crate::EnvironmentProvider;
use crate::EnvironmentProviderFuture;
use crate::ExecServerError;
use crate::client_api::DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT;
use crate::client_api::DEFAULT_REMOTE_EXEC_SERVER_INITIALIZE_TIMEOUT;
use crate::client_api::ExecServerTransportParams;
use crate::client_api::StdioExecServerCommand;
use crate::environment::LOCAL_ENVIRONMENT_ID;
use crate::environment_provider::EnvironmentDefault;
use crate::environment_provider::EnvironmentProviderSnapshot;

const ENVIRONMENTS_TOML_FILE: &str = "environments.toml";
const MAX_ENVIRONMENT_ID_LEN: usize = 64;

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct EnvironmentsToml {
    default: Option<String>,
    include_local: Option<bool>,

    #[serde(default)]
    environments: Vec<EnvironmentToml>,
}

#[derive(Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EnvironmentToml {
    id: String,
    url: Option<String>,
    program: Option<String>,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
    cwd: Option<PathBuf>,
    #[serde(default, with = "option_duration_secs")]
    connect_timeout_sec: Option<Duration>,
    #[serde(default, with = "option_duration_secs")]
    initialize_timeout_sec: Option<Duration>,
}

#[derive(Clone, Debug)]
struct TomlEnvironmentProvider {
    default: EnvironmentDefault,
    include_local: bool,
    environments: Vec<(String, ExecServerTransportParams)>,
}

impl TomlEnvironmentProvider {

    fn new_with_config_dir(
        config: EnvironmentsToml,
        config_dir: &Path,
    ) -> Result<Self, ExecServerError> {
        let EnvironmentsToml {
            default,
            include_local,
            environments,
        } = config;
        let include_local = include_local.unwrap_or(true);
        let mut ids = HashSet::new();
        if include_local {
            ids.insert(LOCAL_ENVIRONMENT_ID.to_string());
        }
        let mut parsed_environments = Vec::with_capacity(environments.len());
        for item in environments {
            let (id, transport) = parse_environment_toml(item, config_dir)?;
            if !ids.insert(id.clone()) {
                return Err(ExecServerError::Protocol(format!(
                    "environment id `{id}` is duplicated"
                )));
            }
            parsed_environments.push((id, transport));
        }
        let default = normalize_default_environment_id(default.as_deref(), include_local, &ids)?;
        Ok(Self {
            default,
            include_local,
            environments: parsed_environments,
        })
    }

    async fn snapshot(&self) -> Result<EnvironmentProviderSnapshot, ExecServerError> {
        let mut environments = Vec::with_capacity(self.environments.len());
        for (id, transport_params) in &self.environments {
            environments.push((
                id.clone(),
                Environment::remote_with_transport(
                    transport_params.clone(),
                    /*local_runtime_paths*/ None,
                ),
            ));
        }

        Ok(EnvironmentProviderSnapshot {
            environments,
            default: self.default.clone(),
            include_local: self.include_local,
        })
    }
}

impl EnvironmentProvider for TomlEnvironmentProvider {
    fn snapshot(&self) -> EnvironmentProviderFuture<'_> {
        Box::pin(TomlEnvironmentProvider::snapshot(self))
    }
}

fn parse_environment_toml(
    item: EnvironmentToml,
    config_dir: &Path,
) -> Result<(String, ExecServerTransportParams), ExecServerError> {
    let EnvironmentToml {
        id,
        url,
        program,
        args,
        env,
        cwd,
        connect_timeout_sec,
        initialize_timeout_sec,
    } = item;
    validate_environment_id(&id)?;
    if program.is_none() && (args.is_some() || env.is_some() || cwd.is_some()) {
        return Err(ExecServerError::Protocol(format!(
            "environment `{id}` args, env, and cwd require program"
        )));
    }
    if url.is_none() && connect_timeout_sec.is_some() {
        return Err(ExecServerError::Protocol(format!(
            "environment `{id}` connect_timeout_sec requires url"
        )));
    }

    let connect_timeout = connect_timeout_sec.unwrap_or(DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT);
    let initialize_timeout =
        initialize_timeout_sec.unwrap_or(DEFAULT_REMOTE_EXEC_SERVER_INITIALIZE_TIMEOUT);

    let transport_params = match (url, program) {
        (Some(url), None) => {
            let url = validate_websocket_url(url)?;
            ExecServerTransportParams::WebSocketUrl {
                websocket_url: url,
                connect_timeout,
                initialize_timeout,
            }
        }
        (None, Some(program)) => {
            let program = program.trim().to_string();
            if program.is_empty() {
                return Err(ExecServerError::Protocol(format!(
                    "environment `{id}` program cannot be empty"
                )));
            }
            ExecServerTransportParams::StdioCommand {
                command: StdioExecServerCommand {
                    program,
                    args: args.unwrap_or_default(),
                    env: env.unwrap_or_default(),
                    // A relative cwd is resolved against the config directory.
                    cwd: cwd.map(|cwd| config_dir.join(cwd)),
                },
                initialize_timeout,
            }
        }
        (None, None) | (Some(_), Some(_)) => {
            return Err(ExecServerError::Protocol(format!(
                "environment `{id}` must set exactly one of url or program"
            )));
        }
    };

    Ok((id, transport_params))
}

pub(crate) fn environment_provider_from_codex_home(
    codex_home: &Path,
) -> Result<Box<dyn EnvironmentProvider>, ExecServerError> {
    let path = codex_home.join(ENVIRONMENTS_TOML_FILE);
    if !path.try_exists().map_err(|err| {
        ExecServerError::Protocol(format!(
            "failed to inspect environment config `{}`: {err}",
            path.display()
        ))
    })? {
        return Ok(Box::new(DefaultEnvironmentProvider::from_env()));
    }

    let environments = load_environments_toml(&path)?;
    Ok(Box::new(TomlEnvironmentProvider::new_with_config_dir(
        environments,
        codex_home,
    )?))
}

fn normalize_default_environment_id(
    default: Option<&str>,
    include_local: bool,
    ids: &HashSet<String>,
) -> Result<EnvironmentDefault, ExecServerError> {
    let Some(default) = default.map(str::trim) else {
        return if include_local {
            Ok(EnvironmentDefault::EnvironmentId(
                LOCAL_ENVIRONMENT_ID.to_string(),
            ))
        } else {
            Ok(EnvironmentDefault::Disabled)
        };
    };
    if default.is_empty() {
        return Err(ExecServerError::Protocol(
            "default environment id cannot be empty".to_string(),
        ));
    }
    if !default.eq_ignore_ascii_case("none") && !ids.contains(default) {
        return Err(ExecServerError::Protocol(format!(
            "default environment `{default}` is not configured"
        )));
    }
    if default.eq_ignore_ascii_case("none") {
        Ok(EnvironmentDefault::Disabled)
    } else {
        Ok(EnvironmentDefault::EnvironmentId(default.to_string()))
    }
}

fn validate_environment_id(id: &str) -> Result<(), ExecServerError> {
    let trimmed_id = id.trim();
    if trimmed_id.is_empty() {
        return Err(ExecServerError::Protocol(
            "environment id cannot be empty".to_string(),
        ));
    }
    if trimmed_id != id {
        return Err(ExecServerError::Protocol(format!(
            "environment id `{id}` must not contain surrounding whitespace"
        )));
    }
    if id == LOCAL_ENVIRONMENT_ID || id.eq_ignore_ascii_case("none") {
        return Err(ExecServerError::Protocol(format!(
            "environment id `{id}` is reserved"
        )));
    }
    if id.len() > MAX_ENVIRONMENT_ID_LEN {
        return Err(ExecServerError::Protocol(format!(
            "environment id `{id}` cannot be longer than {MAX_ENVIRONMENT_ID_LEN} characters"
        )));
    }
    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(ExecServerError::Protocol(format!(
            "environment id `{id}` must contain only ASCII letters, numbers, '-' or '_'"
        )));
    }
    Ok(())
}

fn validate_websocket_url(url: String) -> Result<String, ExecServerError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(ExecServerError::Protocol(
            "environment url cannot be empty".to_string(),
        ));
    }
    if !url.starts_with("ws://") && !url.starts_with("wss://") {
        return Err(ExecServerError::Protocol(format!(
            "environment url `{url}` must use ws:// or wss://"
        )));
    }
    url.into_client_request().map_err(|err| {
        ExecServerError::Protocol(format!("environment url `{url}` is invalid: {err}"))
    })?;
    Ok(url.to_string())
}

fn load_environments_toml(path: &Path) -> Result<EnvironmentsToml, ExecServerError> {
    let contents = std::fs::read_to_string(path).map_err(|err| {
        ExecServerError::Protocol(format!(
            "failed to read environment config `{}`: {err}",
            path.display()
        ))
    })?;

    toml::from_str(&contents).map_err(|err| {
        ExecServerError::Protocol(format!(
            "failed to parse environment config `{}`: {err}",
            path.display()
        ))
    })
}

mod option_duration_secs {
    use std::time::Duration;

    use serde::Deserialize;
    use serde::Deserializer;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = Option::<f64>::deserialize(deserializer)?;
        secs.map(|secs| Duration::try_from_secs_f64(secs).map_err(serde::de::Error::custom))
            .transpose()
    }
}

