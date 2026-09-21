use super::AgentRoleConfig;
use codex_config::ConfigLayerStack;
use codex_config::ConfigLayerStackOrdering;
use codex_config::config_toml::AgentRoleToml;
use codex_config::config_toml::AgentsToml;
use codex_config::config_toml::ConfigToml;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::WalkEntryKind;
use codex_exec_server::WalkOptions;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::AbsolutePathBufGuard;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use toml::Value as TomlValue;

const MAX_AGENT_ROLE_SCAN_DEPTH: usize = 6;
const MAX_AGENT_ROLE_DIRECTORIES: usize = 2_000;
const MAX_AGENT_ROLE_ENTRIES: usize = 20_000;

pub(crate) async fn load_agent_roles(
    fs: &dyn ExecutorFileSystem,
    cfg: &ConfigToml,
    config_layer_stack: &ConfigLayerStack,
    startup_warnings: &mut Vec<String>,
) -> std::io::Result<BTreeMap<String, AgentRoleConfig>> {
    let layers = config_layer_stack.get_layers(
        ConfigLayerStackOrdering::LowestPrecedenceFirst,
        /*include_disabled*/ false,
    );
    if layers.is_empty() {
        return load_agent_roles_without_layers(fs, cfg, startup_warnings).await;
    }

    let mut files = RoleFileCache::default();
    let mut roles: BTreeMap<String, AgentRoleConfig> = BTreeMap::new();
    let mut agent_role_files_by_dir: BTreeMap<AbsolutePathBuf, Vec<AbsolutePathBuf>> =
        BTreeMap::new();
    for layer in layers {
        let mut layer_roles: BTreeMap<String, AgentRoleConfig> = BTreeMap::new();
        let mut declared_role_files = BTreeSet::new();
        let config_folder = layer.config_folder();
        let agents_toml = match agents_toml_from_layer(&layer.config, config_folder.as_deref()) {
            Ok(agents_toml) => agents_toml,
            Err(err) => {
                push_agent_role_warning(startup_warnings, err);
                None
            }
        };
        if let Some(agents_toml) = agents_toml {
            for (declared_role_name, role_toml) in &agents_toml.roles {
                let (role_name, role) =
                    match read_declared_role(fs, &mut files, declared_role_name, role_toml).await {
                        Ok(role) => role,
                        Err(err) => {
                            push_agent_role_warning(startup_warnings, err);
                            continue;
                        }
                    };
                if let Some(config_file) = role.config_file.clone() {
                    declared_role_files.insert(config_file);
                }
                if layer_roles.contains_key(&role_name) {
                    push_agent_role_warning(
                        startup_warnings,
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!(
                                "duplicate agent role name `{role_name}` declared in the same config layer"
                            ),
                        ),
                    );
                    continue;
                }
                layer_roles.insert(role_name, role);
            }
        }

        if let Some(config_folder) = layer.config_folder() {
            let agents_dir = config_folder.join("agents");
            if !agent_role_files_by_dir.contains_key(&agents_dir) {
                let agent_role_files = match collect_agent_role_files(fs, &agents_dir).await {
                    Ok(files) => files,
                    Err(err) => {
                        let message = format!(
                            "Agent role discovery unavailable under {}: {err}",
                            agents_dir.as_path().display()
                        );
                        tracing::warn!("{message}");
                        startup_warnings.push(message);
                        Vec::new()
                    }
                };
                agent_role_files_by_dir.insert(agents_dir.clone(), agent_role_files);
            }
            let agent_role_files = agent_role_files_by_dir.get(&agents_dir);
            let Some(agent_role_files) = agent_role_files else {
                continue;
            };
            for (role_name, role) in discover_agent_roles_in_dir(
                fs,
                &mut files,
                &agents_dir,
                agent_role_files,
                &declared_role_files,
                startup_warnings,
            )
            .await?
            {
                if layer_roles.contains_key(&role_name) {
                    push_agent_role_warning(
                        startup_warnings,
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!(
                                "duplicate agent role name `{role_name}` declared in the same config layer"
                            ),
                        ),
                    );
                    continue;
                }
                layer_roles.insert(role_name, role);
            }
        }

        for (role_name, role) in layer_roles {
            let mut merged_role = role;
            if let Some(existing_role) = roles.get(&role_name) {
                merge_missing_role_fields(&mut merged_role, existing_role);
            }
            roles.insert(role_name, merged_role);
        }
    }

    validate_final_roles(&mut roles, startup_warnings);
    Ok(roles)
}

fn validate_final_roles(
    roles: &mut BTreeMap<String, AgentRoleConfig>,
    startup_warnings: &mut Vec<String>,
) {
    roles.retain(|name, role| {
        match validate_required_agent_role_description(name, role.description.as_deref()) {
            Ok(()) => true,
            Err(err) => {
                push_agent_role_warning(startup_warnings, err);
                false
            }
        }
    });
}

fn push_agent_role_warning(startup_warnings: &mut Vec<String>, err: std::io::Error) {
    let message = format!("Ignoring malformed agent role definition: {err}");
    tracing::warn!("{message}");
    startup_warnings.push(message);
}

async fn load_agent_roles_without_layers(
    fs: &dyn ExecutorFileSystem,
    cfg: &ConfigToml,
    startup_warnings: &mut Vec<String>,
) -> std::io::Result<BTreeMap<String, AgentRoleConfig>> {
    let mut files = RoleFileCache::default();
    let mut roles = BTreeMap::new();
    if let Some(agents_toml) = cfg.agents.as_ref() {
        for (declared_role_name, role_toml) in &agents_toml.roles {
            let (role_name, role) =
                match read_declared_role(fs, &mut files, declared_role_name, role_toml).await {
                    Ok(role) => role,
                    Err(err) => {
                        push_agent_role_warning(startup_warnings, err);
                        continue;
                    }
                };
            if roles.contains_key(&role_name) {
                push_agent_role_warning(
                    startup_warnings,
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("duplicate agent role name `{role_name}` declared in config"),
                    ),
                );
                continue;
            }
            roles.insert(role_name, role);
        }
    }

    validate_final_roles(&mut roles, startup_warnings);
    Ok(roles)
}

async fn read_declared_role(
    fs: &dyn ExecutorFileSystem,
    files: &mut RoleFileCache,
    declared_role_name: &str,
    role_toml: &AgentRoleToml,
) -> std::io::Result<(String, AgentRoleConfig)> {
    let mut role = agent_role_config_from_toml(fs, declared_role_name, role_toml).await?;
    let mut role_name = declared_role_name.to_string();
    if let Some(config_file) = role.config_file.as_deref() {
        let config_file = AbsolutePathBuf::from_absolute_path(config_file)?;
        let parsed_file = files
            .read(fs, &config_file, Some(declared_role_name))
            .await?;
        role_name = parsed_file.role_name;
        role.description = parsed_file.description.or(role.description);
        role.nickname_candidates = parsed_file.nickname_candidates.or(role.nickname_candidates);
    }

    Ok((role_name, role))
}

fn merge_missing_role_fields(role: &mut AgentRoleConfig, fallback: &AgentRoleConfig) {
    role.description = role.description.clone().or(fallback.description.clone());
    role.config_file = role.config_file.clone().or(fallback.config_file.clone());
    role.nickname_candidates = role
        .nickname_candidates
        .clone()
        .or(fallback.nickname_candidates.clone());
}

fn agents_toml_from_layer(
    layer_toml: &TomlValue,
    config_base_dir: Option<&Path>,
) -> std::io::Result<Option<AgentsToml>> {
    let Some(agents_toml) = layer_toml.get("agents") else {
        return Ok(None);
    };

    // AbsolutePathBufGuard resolves relative paths while it remains in scope.
    let _guard = config_base_dir.map(AbsolutePathBufGuard::new);
    agents_toml
        .clone()
        .try_into()
        .map(Some)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

async fn agent_role_config_from_toml(
    fs: &dyn ExecutorFileSystem,
    role_name: &str,
    role: &AgentRoleToml,
) -> std::io::Result<AgentRoleConfig> {
    let config_file = role
        .config_file
        .as_ref()
        .map(AbsolutePathBuf::from_absolute_path)
        .transpose()?;
    validate_agent_role_config_file(fs, role_name, config_file.as_ref()).await?;
    let description = normalize_agent_role_description(
        &format!("agents.{role_name}.description"),
        role.description.as_deref(),
    )?;
    let nickname_candidates = normalize_agent_role_nickname_candidates(
        &format!("agents.{role_name}.nickname_candidates"),
        role.nickname_candidates.as_deref(),
    )?;

    Ok(AgentRoleConfig {
        description,
        config_file: config_file.map(AbsolutePathBuf::into_path_buf),
        nickname_candidates,
    })
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
struct RawAgentRoleFileToml {
    name: Option<String>,
    description: Option<String>,
    nickname_candidates: Option<Vec<String>>,
    #[serde(flatten)]
    config: ConfigToml,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedAgentRoleFile {
    pub(crate) role_name: String,
    pub(crate) description: Option<String>,
    pub(crate) nickname_candidates: Option<Vec<String>>,
    pub(crate) config: TomlValue,
}

pub(crate) fn parse_agent_role_file_contents(
    contents: &str,
    role_file_label: &Path,
    config_base_dir: &Path,
    role_name_hint: Option<&str>,
) -> std::io::Result<ResolvedAgentRoleFile> {
    let role_file_toml: TomlValue = toml::from_str(contents).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "failed to parse agent role file at {}: {err}",
                role_file_label.display()
            ),
        )
    })?;
    let _guard = AbsolutePathBufGuard::new(config_base_dir);
    let parsed: RawAgentRoleFileToml = role_file_toml.clone().try_into().map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "failed to deserialize agent role file at {}: {err}",
                role_file_label.display()
            ),
        )
    })?;
    let description = normalize_agent_role_description(
        &format!("agent role file {}.description", role_file_label.display()),
        parsed.description.as_deref(),
    )?;
    validate_agent_role_file_developer_instructions(
        role_file_label,
        parsed.config.developer_instructions.as_deref(),
        role_name_hint.is_none(),
    )?;

    let role_name = parsed
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| role_name_hint.map(ToOwned::to_owned))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "agent role file at {} must define a non-empty `name`",
                    role_file_label.display()
                ),
            )
        })?;

    let nickname_candidates = normalize_agent_role_nickname_candidates(
        &format!(
            "agent role file {}.nickname_candidates",
            role_file_label.display()
        ),
        parsed.nickname_candidates.as_deref(),
    )?;

    let mut config = role_file_toml;
    let Some(config_table) = config.as_table_mut() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "agent role file at {} must contain a TOML table",
                role_file_label.display()
            ),
        ));
    };
    config_table.remove("name");
    config_table.remove("description");
    config_table.remove("nickname_candidates");

    Ok(ResolvedAgentRoleFile {
        role_name,
        description,
        nickname_candidates,
        config,
    })
}

type CachedRoleResult<T> = Result<T, (ErrorKind, String)>;

/// A single configuration load observes each role file once; name hints remain
/// part of parsing identity because declared and discovered roles differ.
#[derive(Default)]
struct RoleFileCache {
    contents: BTreeMap<AbsolutePathBuf, CachedRoleResult<String>>,
    parsed: BTreeMap<(AbsolutePathBuf, Option<String>), CachedRoleResult<ResolvedAgentRoleFile>>,
}

impl RoleFileCache {
    async fn read(
        &mut self,
        fs: &dyn ExecutorFileSystem,
        path: &AbsolutePathBuf,
        role_name_hint: Option<&str>,
    ) -> std::io::Result<ResolvedAgentRoleFile> {
        let key = (path.clone(), role_name_hint.map(str::to_string));
        if let Some(result) = self.parsed.get(&key) {
            return result
                .clone()
                .map_err(|(kind, message)| std::io::Error::new(kind, message));
        }
        if !self.contents.contains_key(path) {
            let contents = fs
                .read_file_text(&PathUri::from_abs_path(path), None)
                .await
                .map_err(|error| (error.kind(), error.to_string()));
            self.contents.insert(path.clone(), contents);
        }
        let result = match &self.contents[path] {
            Ok(contents) => {
                let base = path.parent().unwrap_or_else(|| path.clone());
                parse_agent_role_file_contents(
                    contents,
                    path.as_path(),
                    base.as_path(),
                    role_name_hint,
                )
                .map_err(|error| (error.kind(), error.to_string()))
            }
            Err(error) => Err(error.clone()),
        };
        self.parsed.insert(key, result.clone());
        result.map_err(|(kind, message)| std::io::Error::new(kind, message))
    }
}

fn normalize_agent_role_description(
    field_label: &str,
    description: Option<&str>,
) -> std::io::Result<Option<String>> {
    match description.map(str::trim) {
        Some("") => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{field_label} cannot be blank"),
        )),
        Some(description) => Ok(Some(description.to_string())),
        None => Ok(None),
    }
}

fn validate_required_agent_role_description(
    role_name: &str,
    description: Option<&str>,
) -> std::io::Result<()> {
    if description.is_some() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("agent role `{role_name}` must define a description"),
        ))
    }
}

fn validate_agent_role_file_developer_instructions(
    role_file_label: &Path,
    developer_instructions: Option<&str>,
    require_present: bool,
) -> std::io::Result<()> {
    match developer_instructions.map(str::trim) {
        Some("") => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "agent role file at {}.developer_instructions cannot be blank",
                role_file_label.display()
            ),
        )),
        Some(_) => Ok(()),
        None if require_present => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "agent role file at {} must define `developer_instructions`",
                role_file_label.display()
            ),
        )),
        None => Ok(()),
    }
}

async fn validate_agent_role_config_file(
    fs: &dyn ExecutorFileSystem,
    role_name: &str,
    config_file: Option<&AbsolutePathBuf>,
) -> std::io::Result<()> {
    let Some(config_file) = config_file else {
        return Ok(());
    };

    let config_file_uri = PathUri::from_abs_path(config_file);
    let metadata = fs
        .get_metadata(&config_file_uri, /*sandbox*/ None)
        .await
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "agents.{role_name}.config_file must point to an existing file at {}: {e}",
                    config_file.as_path().display()
                ),
            )
        })?;
    if metadata.is_file {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "agents.{role_name}.config_file must point to a file: {}",
                config_file.as_path().display()
            ),
        ))
    }
}

fn normalize_agent_role_nickname_candidates(
    field_label: &str,
    nickname_candidates: Option<&[String]>,
) -> std::io::Result<Option<Vec<String>>> {
    let Some(nickname_candidates) = nickname_candidates else {
        return Ok(None);
    };

    if nickname_candidates.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{field_label} must contain at least one name"),
        ));
    }

    let mut normalized_candidates = Vec::with_capacity(nickname_candidates.len());
    let mut seen_candidates = BTreeSet::new();

    for nickname in nickname_candidates {
        let normalized_nickname = nickname.trim();
        if normalized_nickname.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{field_label} cannot contain blank names"),
            ));
        }

        if !seen_candidates.insert(normalized_nickname.to_owned()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{field_label} cannot contain duplicates"),
            ));
        }

        if !normalized_nickname
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "{field_label} may only contain ASCII letters, digits, spaces, hyphens, and underscores"
                ),
            ));
        }

        normalized_candidates.push(normalized_nickname.to_owned());
    }

    Ok(Some(normalized_candidates))
}

async fn discover_agent_roles_in_dir(
    fs: &dyn ExecutorFileSystem,
    files: &mut RoleFileCache,
    agents_dir: &AbsolutePathBuf,
    agent_role_files: &[AbsolutePathBuf],
    declared_role_files: &BTreeSet<PathBuf>,
    startup_warnings: &mut Vec<String>,
) -> std::io::Result<BTreeMap<String, AgentRoleConfig>> {
    let mut roles = BTreeMap::new();

    for agent_file in agent_role_files {
        if declared_role_files.contains(agent_file.as_path()) {
            continue;
        }
        let parsed_file = match files.read(fs, agent_file, /*role_name_hint*/ None).await {
            Ok(parsed_file) => parsed_file,
            Err(err) => {
                push_agent_role_warning(startup_warnings, err);
                continue;
            }
        };
        let role_name = parsed_file.role_name;
        if roles.contains_key(&role_name) {
            push_agent_role_warning(
                startup_warnings,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "duplicate agent role name `{role_name}` discovered in {}",
                        agents_dir.as_path().display()
                    ),
                ),
            );
            continue;
        }
        roles.insert(
            role_name,
            AgentRoleConfig {
                description: parsed_file.description,
                config_file: Some(agent_file.to_path_buf()),
                nickname_candidates: parsed_file.nickname_candidates,
            },
        );
    }

    Ok(roles)
}

async fn collect_agent_role_files(
    fs: &dyn ExecutorFileSystem,
    dir: &AbsolutePathBuf,
) -> std::io::Result<Vec<AbsolutePathBuf>> {
    let root = PathUri::from_abs_path(dir);
    let walk = match fs
        .walk(
            &root,
            WalkOptions {
                max_depth: MAX_AGENT_ROLE_SCAN_DEPTH,
                max_directories: MAX_AGENT_ROLE_DIRECTORIES,
                max_entries: MAX_AGENT_ROLE_ENTRIES,
                follow_directory_symlinks: true,
                prune_hidden_directories: false,
            },
            /*sandbox*/ None,
        )
        .await
    {
        Ok(walk) => walk,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    if walk.truncated {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "agent role discovery reached its traversal limit under {}",
                dir.as_path().display()
            ),
        ));
    }
    if !walk.errors.is_empty() {
        let details = walk
            .errors
            .iter()
            .map(|error| format!("{}: {}", error.path, error.message))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(std::io::Error::other(format!(
            "agent role discovery was incomplete under {}: {details}",
            dir.as_path().display()
        )));
    }

    let mut files = walk
        .entries
        .into_iter()
        .filter(|entry| entry.kind == WalkEntryKind::File)
        .map(|entry| entry.path.to_abs_path())
        .collect::<std::io::Result<Vec<_>>>()?;
    files.retain(|path| {
        path.as_path()
            .extension()
            .is_some_and(|extension| extension == "toml")
    });
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn role_file_cache_preserves_name_hints_and_refreshes_between_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = AbsolutePathBuf::from_absolute_path(dir.path().join("role.toml")).unwrap();
        std::fs::write(&path, "description = 'first'\n").unwrap();
        let fs = codex_exec_server::LOCAL_FS.as_ref();
        let mut cache = RoleFileCache::default();
        let first = cache.read(fs, &path, Some("one")).await.unwrap();
        std::fs::write(&path, "description = 'changed'\n").unwrap();
        assert_eq!(cache.read(fs, &path, Some("one")).await.unwrap(), first);
        let second = cache.read(fs, &path, Some("two")).await.unwrap();
        assert_eq!(second.role_name, "two");
        assert_eq!(second.description.as_deref(), Some("first"));
        assert!(
            cache.read(fs, &path, None).await.is_err(),
            "discovery requires instructions and a name"
        );
        assert_eq!(cache.contents.len(), 1);
        assert_eq!(cache.parsed.len(), 3);
        assert_eq!(
            RoleFileCache::default()
                .read(fs, &path, Some("one"))
                .await
                .unwrap()
                .description
                .as_deref(),
            Some("changed")
        );
    }
    use codex_config::ConfigLayerEntry;
    use codex_config::ConfigLayerSource;

    #[tokio::test]
    async fn final_description_preserves_lower_layer_file() -> std::io::Result<()> {
        let home = tempfile::tempdir()?;
        let path = home.path().join("worker.toml");
        std::fs::write(&path, "developer_instructions = 'do the work'\n")?;
        let low =
            toml::Value::try_from(serde_json::json!({"agents": {"worker": {"config_file": path}}}))
                .unwrap();
        let high = toml::toml! { [agents.worker] description = "Worker role" }.into();
        let stack = ConfigLayerStack::new(
            vec![
                ConfigLayerEntry::new(
                    ConfigLayerSource::User {
                        file: AbsolutePathBuf::from_absolute_path(home.path().join("config.toml"))?,
                        profile: None,
                    },
                    low,
                ),
                ConfigLayerEntry::new(ConfigLayerSource::SessionFlags, high),
            ],
            Default::default(),
            Default::default(),
        )?;
        let mut warnings = Vec::new();
        let roles = load_agent_roles(
            crate::config::LOCAL_FS.as_ref(),
            &ConfigToml::default(),
            &stack,
            &mut warnings,
        )
        .await?;
        assert_eq!(roles["worker"].description.as_deref(), Some("Worker role"));
        assert_eq!(roles["worker"].config_file.as_ref(), Some(&path));
        assert!(warnings.is_empty(), "{warnings:?}");
        Ok(())
    }

    #[tokio::test]
    async fn no_layer_loading_retains_valid_roles_and_reports_invalid_once() -> std::io::Result<()>
    {
        let cfg: ConfigToml =
            toml::from_str("[agents.valid]\ndescription = 'valid'\n[agents.incomplete]\n").unwrap();
        let mut warnings = Vec::new();
        let roles = load_agent_roles(
            crate::config::LOCAL_FS.as_ref(),
            &cfg,
            &ConfigLayerStack::default(),
            &mut warnings,
        )
        .await?;
        assert_eq!(
            roles.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["valid"]
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("incomplete"));
        Ok(())
    }
}
