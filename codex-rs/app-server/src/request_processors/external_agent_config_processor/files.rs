//! Filesystem side of the legacy wire adapter; runs only on a blocking worker.
use codex_app_server_protocol::*;
use codex_file_system::{AtomicWriteLock, resolve_symlink_write_paths, try_acquire_atomic_write_lock, write_bytes_atomically_preserving_permissions};
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;
use toml::Value as TomlValue;
use toml_edit::{DocumentMut, Item as TomlItem};
use super::formats;

const EXTERNAL_AGENT_MCP_CONFIG_FILE: &str = ".mcp.json";

pub(super) fn detect(home: &Path, params: ExternalAgentConfigDetectParams) -> io::Result<Vec<ExternalAgentConfigMigrationItem>> {
    let mut scopes = Vec::new();
    if params.include_home {
        scopes.push(None);
    }
    let mut roots = HashSet::new();
    for cwd in params.cwds.unwrap_or_default() {
        if let Some(root) = repo_root(&cwd)?
            && roots.insert(root.clone())
        {
            scopes.push(Some(root));
        }
    }
    let mut items = Vec::new();
    for cwd in scopes {
        let paths = Paths::new(home, cwd.as_deref());
        let settings = settings(&paths.source)?;
        let migrated = config_from_settings(settings.as_ref())?;
        if !migrated.as_table().is_some_and(|table| table.is_empty()) {
            let mut existing = read_toml(&paths.config)?;
            if merge_missing(&mut existing, &migrated)? {
                items.push(item(ExternalAgentConfigMigrationItemType::Config, cwd.clone(), None,
                    format!("Migrate {} into {}", paths.source.join("settings.json").display(), paths.config.display())));
            }
        }
        let (mcp, _) = migrated_mcp(&paths, settings.as_ref())?;
        let names = if mcp.get("mcp_servers").is_some() {
            missing_mcp(&read_toml(&paths.config)?, &mcp)?
        } else {
            Vec::new()
        };
        if !names.is_empty() {
            items.push(item(ExternalAgentConfigMigrationItemType::McpServerConfig, cwd.clone(),
                Some(MigrationDetails { mcp_servers: names.into_iter().map(|name| McpServerMigration { name }).collect(), ..Default::default() }),
                format!("Migrate MCP servers into {}", paths.config.display())));
        }
        let names = missing_skills(&paths)?;
        if !names.is_empty() {
            items.push(item(ExternalAgentConfigMigrationItemType::Skills, cwd.clone(),
                Some(MigrationDetails { skills: names.into_iter().map(|name| SkillMigration { name }).collect(), ..Default::default() }),
                format!("Migrate skills into {}", paths.skills.display())));
        }
        if let Some(source) = paths.instructions_source()?
            && empty_text(&paths.instructions)?
            && !empty_text(&source)?
        {
            items.push(item(ExternalAgentConfigMigrationItemType::AgentsMd, cwd.clone(), None,
                format!("Migrate {} into {}", source.display(), paths.instructions.display())));
        }
        let target_config_dir = paths.config.parent().ok_or_else(|| invalid_data_error("config target has no parent"))?;
        for (kind, subdir, agents) in [
            (ExternalAgentConfigMigrationItemType::Subagents, "agents", true),
            (ExternalAgentConfigMigrationItemType::Commands, "commands", false),
        ] {
            let target = if agents { target_config_dir.join("agents") } else { paths.skills.clone() };
            let plans = formats::documents(&paths.source.join(subdir), &target, agents)?.plans;
            if !plans.is_empty() {
                let details = if agents {
                    MigrationDetails { subagents: plans.into_iter().map(|plan| SubagentMigration { name: plan.name }).collect(), ..Default::default() }
                } else {
                    MigrationDetails { commands: plans.into_iter().map(|plan| CommandMigration { name: plan.name }).collect(), ..Default::default() }
                };
                items.push(item(kind, cwd.clone(), Some(details), format!("Migrate {subdir} into {}", target.display())));
            }
        }
        if empty_text(&target_config_dir.join("hooks.json"))? {
            let (names, _, _) = formats::hooks(&paths.source, target_config_dir)?;
            if !names.is_empty() {
                items.push(item(ExternalAgentConfigMigrationItemType::Hooks, cwd.clone(),
                    Some(MigrationDetails { hooks: names.into_iter().map(|name| HookMigration { name }).collect(), ..Default::default() }),
                    format!("Migrate hooks into {}", target_config_dir.display())));
            }
        }
        if cwd.is_none() {
            let sessions = super::sessions::detect_recent_sessions(&paths.external_home, home)?;
            if !sessions.is_empty() {
                items.push(item(ExternalAgentConfigMigrationItemType::Sessions, None,
                    Some(MigrationDetails { sessions, ..Default::default() }), "Import recent external-agent sessions".to_string()));
            }
        }
        if let Some(plugins) = settings.as_ref().and_then(|value| value.get("enabledPlugins")).and_then(JsonValue::as_object) {
            let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (plugin, enabled) in plugins {
                if enabled.as_bool() == Some(true)
                    && let Some((name, marketplace)) = plugin.rsplit_once('@')
                {
                    groups.entry(marketplace.to_string()).or_default().push(name.to_string());
                }
            }
            if !groups.is_empty() {
                items.push(item(ExternalAgentConfigMigrationItemType::Plugins, cwd.clone(),
                    Some(MigrationDetails { plugins: groups.into_iter().map(|(marketplace_name, plugin_names)| PluginsMigration { marketplace_name, plugin_names }).collect(), ..Default::default() }),
                    "Migrate enabled external-agent plugins".to_string()));
            }
        }
    }
    Ok(items)
}

pub(super) fn import(home: &Path, items: Vec<ExternalAgentConfigMigrationItem>, cancellation: &CancellationToken) -> Vec<ExternalAgentConfigImportTypeResult> {
    let mut results: Vec<ExternalAgentConfigImportTypeResult> = Vec::new();
    for item in items {
        let mut result = ExternalAgentConfigImportTypeResult { item_type: item.item_type, successes: Vec::new(), failures: Vec::new() };
        if let Err(err) = import_item(home, &item, cancellation, &mut result) {
            let unsupported = err.kind() == io::ErrorKind::Unsupported;
            let message = err.to_string();
            let error_type = if unsupported { "unsupported_migration" }
                else if message.contains("invalid existing config.toml") { "invalid_existing_config" }
                else { "external_agent_config_import_error" };
            result.failures.push(ExternalAgentConfigImportItemTypeFailure {
                item_type: item.item_type,
                error_type: Some(error_type.to_string()),
                failure_stage: if unsupported { "compatibility_adapter" } else { "import_request_failed" }.to_string(),
                message, cwd: item.cwd.clone(), source: None,
            });
        }
        if let Some(existing) = results.iter_mut().find(|existing| existing.item_type == item.item_type) {
            existing.successes.extend(result.successes);
            existing.failures.extend(result.failures);
        } else {
            results.push(result);
        }
    }
    results.sort_by_key(|result| match result.item_type {
        ExternalAgentConfigMigrationItemType::Config => 0,
        ExternalAgentConfigMigrationItemType::Skills => 1,
        ExternalAgentConfigMigrationItemType::AgentsMd => 2,
        ExternalAgentConfigMigrationItemType::Plugins => 3,
        ExternalAgentConfigMigrationItemType::McpServerConfig => 4,
        ExternalAgentConfigMigrationItemType::Subagents => 5,
        ExternalAgentConfigMigrationItemType::Hooks => 6,
        ExternalAgentConfigMigrationItemType::Commands => 7,
        ExternalAgentConfigMigrationItemType::Sessions => 8,
    });
    results
}

fn import_item(home: &Path, item: &ExternalAgentConfigMigrationItem, cancellation: &CancellationToken, result: &mut ExternalAgentConfigImportTypeResult) -> io::Result<()> {
    use ExternalAgentConfigMigrationItemType::*;
    check_cancel(cancellation)?;
    if matches!(item.item_type, Plugins | Sessions) {
        return Err(invalid_data_error("plugin and session imports require the asynchronous adapter"));
    }
    let cwd = match item.cwd.as_deref().filter(|cwd| !cwd.as_os_str().is_empty()) {
        Some(cwd) => match repo_root(cwd)? { Some(root) => Some(root), None => return Ok(()) },
        None => None,
    };
    let paths = Paths::new(home, cwd.as_deref());
    match item.item_type {
        Config | McpServerConfig => {
            let settings = settings(&paths.source)?;
            let (migrated, failures) = if item.item_type == Config {
                (config_from_settings(settings.as_ref())?, Vec::new())
            } else {
                migrated_mcp(&paths, settings.as_ref())?
            };
            for (name, message) in failures {
                result.failures.push(ExternalAgentConfigImportItemTypeFailure {
                    item_type: item.item_type, error_type: Some("unsupported_mcp_server".to_string()),
                    failure_stage: "mcp_conversion".to_string(), message, cwd: item.cwd.clone(), source: Some(name),
                });
            }
            if migrated.as_table().is_some_and(|table| table.is_empty()) { return Ok(()); }
            let (target, _lock) = lock_target(&paths.config, cancellation)?;
            let serialized = match fs::read_to_string(&target) {
                Ok(serialized) => serialized,
                Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
                Err(err) => return Err(err),
            };
            let mut raw_values = Vec::new();
            let mut existing = parse_config_document(&serialized, &mut raw_values)?;
            // Serialize only the new imported values. The target is always
            // edited as a document; its existing nodes are never reconstructed.
            let mut migration = toml::to_string_pretty(&migrated).map_err(io::Error::other)?;
            if default_crlf(&serialized) {
                migration = migration.replace("\r\n", "\n").replace('\n', "\r\n");
            }
            let mut migration = parse_config_document(&migration, &mut raw_values)?;
            clear_imported_table_positions(migration.as_item_mut());
            let mut names = Vec::new();
            let changed = if item.item_type == McpServerConfig {
                let servers = migration.get("mcp_servers").ok_or_else(|| invalid_data_error("migration is missing mcp_servers"))?;
                let target_servers = existing.as_table_mut().entry("mcp_servers").or_insert_with(|| {
                    let mut table = toml_edit::Table::new();
                    table.set_implicit(true);
                    TomlItem::Table(table)
                });
                let current = target_servers.as_table_like().ok_or_else(|| invalid_data_error("mcp_servers must be a TOML table"))?;
                names = servers.as_table_like().ok_or_else(|| invalid_data_error("migrated mcp_servers must be a TOML table"))?
                    .iter().filter(|(name, _)| !current.contains_key(name)).map(|(name, _)| name.to_string()).collect();
                // Existing servers are atomic: do not fill or replace their
                // individual fields, even when the source has more settings.
                merge_missing_document(target_servers, servers, false)?
            } else { merge_missing_document(existing.as_item_mut(), migration.as_item(), true)? };
            if changed {
                let rendered = render_config_document(&serialized, &existing, &raw_values)?;
                check_cancel(cancellation)?;
                write_bytes_atomically_preserving_permissions(&target, rendered.as_bytes())?;
                if item.item_type == McpServerConfig {
                    for name in names { success(result, item, name.clone(), name); }
                } else {
                    success(result, item, paths.source.join("settings.json").display().to_string(), paths.config.display().to_string());
                }
            }
        }
        AgentsMd => {
            let Some(source) = paths.instructions_source()? else { return Ok(()); };
            if empty_text(&source)? { return Ok(()); }
            let (target, _lock) = lock_target(&paths.instructions, cancellation)?;
            if empty_text(&target)? {
                let content = rewrite_terms(&fs::read_to_string(&source)?);
                check_cancel(cancellation)?;
                write_bytes_atomically_preserving_permissions(&target, content.as_bytes())?;
                success(result, item, source.display().to_string(), paths.instructions.display().to_string());
            }
        }
        Skills => {
            // The historical API selected the category, not individual entries:
            // details.skills is discovery metadata; import all missing folders.
            for name in missing_skills(&paths)? {
                check_cancel(cancellation)?;
                fs::create_dir_all(&paths.skills)?;
                let staging = tempfile::Builder::new().prefix(".skill-import-").tempdir_in(&paths.skills)?;
                let staged = staging.path().join(&name);
                copy_skill(&paths.source.join("skills").join(&name), &staged, cancellation)?;
                let (target, _lock) = lock_target(&paths.skills.join(&name), cancellation)?;
                if !target.try_exists()? {
                    check_cancel(cancellation)?;
                    fs::rename(&staged, &target)?;
                    success(result, item, name.clone(), name);
                }
            }
        }
        Subagents | Commands => {
            let agents = item.item_type == Subagents;
            let source = paths.source.join(if agents { "agents" } else { "commands" });
            let target = if agents { paths.config.parent().ok_or_else(|| invalid_data_error("config has no parent"))?.join("agents") } else { paths.skills.clone() };
            let conversions = formats::documents(&source, &target, agents)?;
            for (source, message) in conversions.failures {
                result.failures.push(ExternalAgentConfigImportItemTypeFailure {
                    item_type: item.item_type, error_type: Some("unsupported_source_document".to_string()),
                    failure_stage: "document_conversion".to_string(), message,
                    cwd: item.cwd.clone(), source: Some(source.display().to_string()),
                });
            }
            for plan in conversions.plans {
                let (destination, _lock) = lock_target(&plan.target, cancellation)?;
                if !destination.try_exists()? {
                    check_cancel(cancellation)?;
                    if formats::publish_new_file(&destination, plan.contents.as_bytes(), None)? {
                        success(result, item, plan.name.clone(), plan.name);
                    }
                }
            }
        }
        Hooks => {
            let target_dir = paths.config.parent().ok_or_else(|| invalid_data_error("config has no parent"))?;
            let (names, rendered, failures) = formats::hooks(&paths.source, target_dir)?;
            for (source, message) in failures {
                result.failures.push(ExternalAgentConfigImportItemTypeFailure {
                    item_type: item.item_type, error_type: Some("unsupported_hook".to_string()),
                    failure_stage: "hook_conversion".to_string(), message,
                    cwd: item.cwd.clone(), source: Some(source.display().to_string()),
                });
            }
            if !names.is_empty() {
                let (target, _lock) = lock_target(&target_dir.join("hooks.json"), cancellation)?;
                if empty_text(&target)? {
                    let was_missing = !target.try_exists()?;
                    formats::copy_hook_scripts(&paths.source, target_dir, cancellation)?;
                    check_cancel(cancellation)?;
                    let published = if was_missing {
                        formats::publish_new_file(&target, rendered.as_bytes(), None)?
                    } else {
                        write_bytes_atomically_preserving_permissions(&target, rendered.as_bytes())?;
                        true
                    };
                    if published {
                        for name in names { success(result, item, name.clone(), name); }
                    }
                }
            }
        }
        _ => unreachable!("asynchronous categories handled by the processor"),
    }
    Ok(())
}

fn success(result: &mut ExternalAgentConfigImportTypeResult, item: &ExternalAgentConfigMigrationItem, source: String, target: String) {
    result.successes.push(ExternalAgentConfigImportItemTypeSuccess { item_type: item.item_type, cwd: item.cwd.clone(), source: Some(source), target: Some(target) });
}

fn item(item_type: ExternalAgentConfigMigrationItemType, cwd: Option<PathBuf>, details: Option<MigrationDetails>, description: String) -> ExternalAgentConfigMigrationItem {
    ExternalAgentConfigMigrationItem { item_type, cwd, details, description }
}

struct Paths {
    source: PathBuf,
    external_home: PathBuf,
    root: PathBuf,
    config: PathBuf,
    skills: PathBuf,
    instructions: PathBuf,
    repo: Option<PathBuf>,
}

impl Paths {
    fn new(home: &Path, repo: Option<&Path>) -> Self {
        let external_home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(".claude");
        let source = repo.map(|root| root.join(".claude")).unwrap_or_else(|| external_home.clone());
        let target = repo.map(|root| root.join(".codex")).unwrap_or_else(|| home.to_path_buf());
        let skills = repo.map(|root| root.join(".agents").join("skills")).unwrap_or_else(|| home.parent().unwrap_or_else(|| Path::new(".")).join(".agents").join("skills"));
        let root = repo.map(Path::to_path_buf).unwrap_or_else(|| external_home.parent().unwrap_or_else(|| Path::new(".")).to_path_buf());
        Self { source, external_home, root, config: target.join("config.toml"), skills, instructions: repo.unwrap_or(&target).join("AGENTS.md"), repo: repo.map(Path::to_path_buf) }
    }

    fn instructions_source(&self) -> io::Result<Option<PathBuf>> {
        if let Some(repo) = self.repo.as_ref() {
            for candidate in [repo.join("CLAUDE.md"), self.source.join("CLAUDE.md")] {
                if candidate.is_file() && !empty_text(&candidate)? { return Ok(Some(candidate)); }
            }
            Ok(None)
        } else {
            let candidate = self.source.join("CLAUDE.md");
            Ok(candidate.is_file().then_some(candidate))
        }
    }
}

fn repo_root(cwd: &Path) -> io::Result<Option<PathBuf>> {
    if cwd.as_os_str().is_empty() { return Ok(None); }
    let mut current = if cwd.is_absolute() { cwd.to_path_buf() } else { std::env::current_dir()?.join(cwd) };
    if !current.try_exists()? { return Ok(None); }
    if current.is_file() { current.pop(); }
    let fallback = current.clone();
    loop {
        if current.join(".git").exists() { return Ok(Some(current)); }
        if !current.pop() { return Ok(Some(fallback)); }
    }
}

fn settings(source: &Path) -> io::Result<Option<JsonValue>> {
    let mut base = read_json(&source.join("settings.json"))?;
    match read_json(&source.join("settings.local.json")) {
        Ok(Some(local)) => match base.as_mut() { Some(base) => merge_json(base, &local), None => base = Some(local) },
        Ok(None) => {},
        Err(err) if err.kind() == io::ErrorKind::InvalidData => {},
        Err(err) => return Err(err),
    }
    Ok(base)
}

fn read_json(path: &Path) -> io::Result<Option<JsonValue>> {
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
        _ => {},
    }
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map(Some).map_err(|err| invalid_data_error(err.to_string())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn merge_json(base: &mut JsonValue, incoming: &JsonValue) {
    if let (Some(base), Some(incoming)) = (base.as_object_mut(), incoming.as_object()) {
        for (key, value) in incoming {
            if let Some(existing) = base.get_mut(key) { merge_json(existing, value); } else { base.insert(key.clone(), value.clone()); }
        }
    } else { *base = incoming.clone(); }
}

fn config_from_settings(settings: Option<&JsonValue>) -> io::Result<TomlValue> {
    let mut root = toml::map::Map::new();
    let Some(settings) = settings else { return Ok(TomlValue::Table(root)); };
    let settings = settings.as_object().ok_or_else(|| invalid_data_error("external agent settings root must be an object"))?;
    if let Some(env) = settings.get("env").and_then(JsonValue::as_object).filter(|env| !env.is_empty()) {
        let env = env.iter().filter_map(|(key, value)| json_string(value).map(|value| (key.clone(), TomlValue::String(value)))).collect();
        let mut policy = toml::map::Map::new();
        policy.insert("inherit".to_string(), TomlValue::String("core".to_string()));
        policy.insert("set".to_string(), TomlValue::Table(env));
        root.insert("shell_environment_policy".to_string(), TomlValue::Table(policy));
    }
    if settings.get("sandbox").and_then(|sandbox| sandbox.get("enabled")).and_then(JsonValue::as_bool) == Some(true) {
        root.insert("sandbox_mode".to_string(), TomlValue::String("workspace-write".to_string()));
    }
    Ok(TomlValue::Table(root))
}

fn read_toml(path: &Path) -> io::Result<TomlValue> {
    match fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => Ok(TomlValue::Table(Default::default())),
        Ok(raw) => toml::from_str(&raw).map_err(|err| invalid_data_error(format!("invalid existing config.toml: {err}"))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(TomlValue::Table(Default::default())),
        Err(err) => Err(err),
    }
}

fn merge_missing(existing: &mut TomlValue, incoming: &TomlValue) -> io::Result<bool> {
    let existing = existing.as_table_mut().ok_or_else(|| invalid_data_error("config must be a TOML table"))?;
    let incoming = incoming.as_table().ok_or_else(|| invalid_data_error("migration must be a TOML table"))?;
    let mut changed = false;
    for (key, incoming) in incoming {
        if let Some(value) = existing.get_mut(key) {
            if value.is_table() && incoming.is_table() { changed |= merge_missing(value, incoming)?; }
        } else { existing.insert(key.clone(), incoming.clone()); changed = true; }
    }
    Ok(changed)
}

fn migrated_mcp(paths: &Paths, local_settings: Option<&JsonValue>) -> io::Result<(TomlValue, Vec<(String, String)>)> {
    let fallback;
    let effective = if paths.repo.is_some() && local_settings.is_none() {
        fallback = match settings(&paths.external_home) {
            Ok(settings) => settings,
            Err(err) => { tracing::warn!(error = %err, "ignoring invalid external agent home settings during repo MCP migration"); None },
        };
        fallback.as_ref()
    } else { local_settings };
    build_mcp_config_from_external(&paths.root, Some(&paths.external_home), effective)
}

fn missing_mcp(existing: &TomlValue, incoming: &TomlValue) -> io::Result<Vec<String>> {
    if let Some(value) = existing.get("mcp_servers") && !value.is_table() {
        return Err(invalid_data_error("mcp_servers must be a TOML table"));
    }
    Ok(incoming.get("mcp_servers").and_then(TomlValue::as_table).into_iter().flat_map(|table| table.keys())
        .filter(|name| existing.get("mcp_servers").and_then(|servers| servers.get(name.as_str())).is_none()).cloned().collect())
}

fn missing_skills(paths: &Paths) -> io::Result<Vec<String>> {
    let source = paths.source.join("skills");
    if !source.is_dir() { return Ok(Vec::new()); }
    let mut names = Vec::new();
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !paths.skills.join(entry.file_name()).try_exists()? {
            names.push(entry.file_name().into_string().map_err(|_| invalid_data_error("skill name is not UTF-8"))?);
        }
    }
    names.sort();
    Ok(names)
}

fn copy_skill(source: &Path, target: &Path, cancellation: &CancellationToken) -> io::Result<()> {
    check_cancel(cancellation)?;
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        check_cancel(cancellation)?;
        let entry = entry?;
        let kind = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if kind.is_dir() { copy_skill(&entry.path(), &destination, cancellation)?; }
        else if kind.is_file() {
            if entry.file_name().to_string_lossy().eq_ignore_ascii_case("SKILL.md") {
                fs::write(&destination, rewrite_terms(&fs::read_to_string(entry.path())?))?;
                fs::set_permissions(&destination, entry.metadata()?.permissions())?;
            } else { fs::copy(entry.path(), destination)?; }
        } else {
            return Err(io::Error::new(io::ErrorKind::Unsupported, format!("skill contains a symlink or special file: {}", entry.path().display())));
        }
    }
    Ok(())
}

fn empty_text(path: &Path) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => return Ok(false),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(err),
        _ => {},
    }
    match fs::read_to_string(path) {
        Ok(text) => Ok(text.trim().is_empty()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err),
    }
}

pub(super) fn check_cancel(cancellation: &CancellationToken) -> io::Result<()> {
    if cancellation.is_cancelled() { Err(io::Error::new(io::ErrorKind::Interrupted, "external-agent import cancelled")) } else { Ok(()) }
}

fn physical_target(path: &Path) -> io::Result<PathBuf> {
    let paths = resolve_symlink_write_paths(path)?;
    let parent = paths.write_path.parent().ok_or_else(|| invalid_data_error("target has no parent"))?;
    fs::create_dir_all(parent)?;
    Ok(fs::canonicalize(parent)?.join(paths.write_path.file_name().ok_or_else(|| invalid_data_error("target has no file name"))?))
}

pub(super) fn lock_target(path: &Path, cancellation: &CancellationToken) -> io::Result<(PathBuf, AtomicWriteLock)> {
    check_cancel(cancellation)?;
    let target = physical_target(path)?;
    loop {
        check_cancel(cancellation)?;
        if let Some(lock) = try_acquire_atomic_write_lock(&target)? {
            if physical_target(path)? != target { return Err(io::Error::other("target changed while acquiring its persistence lock; retry the import")); }
            return Ok((target, lock));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

pub(super) fn rewrite_terms(content: &str) -> String {
    let mut result = content.to_string();
    for (needle, replacement) in [("claude.md", "AGENTS.md"), ("claude code", "Codex"), ("claude-code", "Codex"), ("claude_code", "Codex"), ("claudecode", "Codex"), ("claude", "Codex")] {
        let lower = result.to_ascii_lowercase();
        let bytes = result.as_bytes();
        let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
        let mut next = String::with_capacity(result.len());
        let mut emitted = 0;
        for (start, _) in lower.match_indices(needle) {
            let end = start + needle.len();
            if (start == 0 || !word(bytes[start - 1])) && (end == bytes.len() || !word(bytes[end])) {
                next.push_str(&result[emitted..start]); next.push_str(replacement); emitted = end;
            }
        }
        next.push_str(&result[emitted..]); result = next;
    }
    result
}

fn invalid_data_error(message: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message.into()) }

fn external_agent_project_config_file() -> String { ".claude.json".to_string() }

fn build_mcp_config_from_external(
    source_root: &Path,
    external_agent_home: Option<&Path>,
    settings: Option<&JsonValue>,
) -> io::Result<(TomlValue, Vec<(String, String)>)> {
    let mcp_servers = read_external_mcp_servers(source_root, external_agent_home)?;
    if mcp_servers.is_empty() {
        return Ok((TomlValue::Table(Default::default()), Vec::new()));
    }

    let enabled_servers = settings
        .and_then(|settings| settings.get("enabledMcpjsonServers"))
        .map(json_string_vec)
        .unwrap_or_default();
    let disabled_servers = settings
        .and_then(|settings| settings.get("disabledMcpjsonServers"))
        .map(json_string_vec)
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>();

    let mut servers = toml::map::Map::new();
    let mut failures = Vec::new();
    for (server_name, server_config) in mcp_servers {
        if server_config.as_object().is_some_and(|config| mcp_server_is_disabled(&server_name, config, &enabled_servers, &disabled_servers)) { continue; }
        if let Some(server) = mcp_server_toml_table(
            &server_name,
            server_config.as_object(),
            &enabled_servers,
            &disabled_servers,
        ) {
            servers.insert(server_name.clone(), TomlValue::Table(server));
        } else {
            failures.push((server_name.clone(), format!("MCP server {server_name:?} uses an unsupported transport, placeholder, or configuration; this server was not imported")));
        }
    }

    if servers.is_empty() {
        return Ok((TomlValue::Table(Default::default()), failures));
    }

    let mut root = toml::map::Map::new();
    root.insert("mcp_servers".to_string(), TomlValue::Table(servers));
    Ok((TomlValue::Table(root), failures))
}

fn read_external_mcp_servers(
    source_root: &Path,
    external_agent_home: Option<&Path>,
) -> io::Result<BTreeMap<String, JsonValue>> {
    let mut servers = BTreeMap::new();
    let project_config_file = external_agent_project_config_file();
    for relative_path in [
        EXTERNAL_AGENT_MCP_CONFIG_FILE.to_string(),
        project_config_file.clone(),
    ] {
        let source_file = source_root.join(&relative_path);
        if !source_file.is_file() {
            continue;
        }
        let raw = fs::read_to_string(&source_file)?;
        let parsed: JsonValue = serde_json::from_str(&raw)
            .map_err(|err| invalid_data_error(format!("invalid MCP config: {err}")))?;
        append_mcp_servers_from_value(&parsed, &mut servers, McpServerMerge::Overwrite);
        if relative_path == project_config_file
            && let Some(projects) = parsed.get("projects").and_then(JsonValue::as_object)
        {
            for (project_path, project_config) in projects {
                if project_path_matches_source_root(project_path, source_root) {
                    append_mcp_servers_from_value(
                        project_config,
                        &mut servers,
                        McpServerMerge::Overwrite,
                    );
                }
            }
        }
    }
    if let Some(external_agent_root) = external_agent_home.and_then(Path::parent)
        && external_agent_root != source_root
    {
        append_external_agent_project_mcp_servers(
            &external_agent_root.join(external_agent_project_config_file()),
            source_root,
            &mut servers,
        )?;
    }

    Ok(servers)
}

fn append_external_agent_project_mcp_servers(
    source_file: &Path,
    source_root: &Path,
    servers: &mut BTreeMap<String, JsonValue>,
) -> io::Result<()> {
    if !source_file.is_file() {
        return Ok(());
    }
    let raw = fs::read_to_string(source_file)?;
    let parsed: JsonValue = serde_json::from_str(&raw)
        .map_err(|err| invalid_data_error(format!("invalid MCP config: {err}")))?;
    let Some(projects) = parsed.get("projects").and_then(JsonValue::as_object) else {
        return Ok(());
    };
    for (project_path, project_config) in projects {
        if project_path_matches_source_root(project_path, source_root) {
            append_mcp_servers_from_value(
                project_config,
                servers,
                McpServerMerge::PreserveExisting,
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum McpServerMerge {
    Overwrite,
    PreserveExisting,
}

fn append_mcp_servers_from_value(
    value: &JsonValue,
    servers: &mut BTreeMap<String, JsonValue>,
    merge: McpServerMerge,
) {
    let Some(mcp_servers) = value.get("mcpServers").and_then(JsonValue::as_object) else {
        return;
    };
    for (server_name, server_config) in mcp_servers {
        match merge {
            McpServerMerge::Overwrite => {
                servers.insert(server_name.clone(), server_config.clone());
            }
            McpServerMerge::PreserveExisting => {
                servers
                    .entry(server_name.clone())
                    .or_insert_with(|| server_config.clone());
            }
        }
    }
}

fn project_path_matches_source_root(project_path: &str, source_root: &Path) -> bool {
    let project_path = Path::new(project_path);
    if project_path == source_root {
        return true;
    }
    let Ok(project_path) = project_path.canonicalize() else {
        return false;
    };
    source_root
        .canonicalize()
        .is_ok_and(|source_root| source_root == project_path)
}

fn mcp_server_toml_table(
    server_name: &str,
    server_config: Option<&serde_json::Map<String, JsonValue>>,
    enabled_servers: &[String],
    disabled_servers: &BTreeSet<String>,
) -> Option<toml::map::Map<String, TomlValue>> {
    let mut table = toml::map::Map::new();
    let server_config = server_config?;
    let transport_type = server_config.get("type").and_then(JsonValue::as_str);
    if mcp_server_is_disabled(
        server_name,
        server_config,
        enabled_servers,
        disabled_servers,
    ) {
        return None;
    }

    if let Some(command) = server_config.get("command").and_then(json_string) {
        if !matches!(transport_type, None | Some("stdio")) {
            return None;
        }
        if contains_env_placeholder(&command) {
            return None;
        }
        table.insert("command".to_string(), TomlValue::String(command));
        if let Some(args) = server_config.get("args") {
            let args = json_string_vec(args);
            if args.iter().any(|arg| contains_env_placeholder(arg)) {
                return None;
            }
            let args = args.into_iter().map(TomlValue::String).collect::<Vec<_>>();
            if !args.is_empty() {
                table.insert("args".to_string(), TomlValue::Array(args));
            }
        }
        if let Some(env) = server_config.get("env").and_then(JsonValue::as_object) {
            append_env_config(&mut table, env)?;
        }
    } else {
        let url = server_config.get("url").and_then(json_string)?;
        if !matches!(
            transport_type,
            None | Some("http") | Some("streamable_http")
        ) {
            return None;
        }
        if contains_env_placeholder(&url) {
            return None;
        }
        table.insert("url".to_string(), TomlValue::String(url));
        if let Some(headers) = server_config.get("headers").and_then(JsonValue::as_object) {
            append_header_config(&mut table, headers)?;
        }
    }

    Some(table)
}

fn mcp_server_is_disabled(
    server_name: &str,
    server_config: &serde_json::Map<String, JsonValue>,
    enabled_servers: &[String],
    disabled_servers: &BTreeSet<String>,
) -> bool {
    server_config
        .get("enabled")
        .and_then(JsonValue::as_bool)
        .is_some_and(|enabled| !enabled)
        || server_config
            .get("disabled")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false)
        || (!enabled_servers.is_empty() && !enabled_servers.iter().any(|name| name == server_name))
        || disabled_servers.contains(server_name)
}

fn append_header_config(
    table: &mut toml::map::Map<String, TomlValue>,
    headers: &serde_json::Map<String, JsonValue>,
) -> Option<()> {
    let mut static_headers = toml::map::Map::new();
    let mut env_headers = toml::map::Map::new();

    for (key, value) in headers {
        let header_value = json_string(value).unwrap_or_else(|| value.to_string());
        if key.eq_ignore_ascii_case("authorization")
            && let Some(token_env) = header_value
                .strip_prefix("Bearer ")
                .and_then(parse_env_placeholder)
        {
            table.insert(
                "bearer_token_env_var".to_string(),
                TomlValue::String(token_env),
            );
            continue;
        }

        if let Some(env_var) = parse_env_placeholder(&header_value) {
            env_headers.insert(key.clone(), TomlValue::String(env_var));
        } else if contains_env_placeholder(&header_value) {
            return None;
        } else {
            static_headers.insert(key.clone(), TomlValue::String(header_value));
        }
    }

    if !static_headers.is_empty() {
        table.insert("http_headers".to_string(), TomlValue::Table(static_headers));
    }
    if !env_headers.is_empty() {
        table.insert(
            "env_http_headers".to_string(),
            TomlValue::Table(env_headers),
        );
    }
    Some(())
}

fn append_env_config(
    table: &mut toml::map::Map<String, TomlValue>,
    env: &serde_json::Map<String, JsonValue>,
) -> Option<()> {
    let mut static_env = toml::map::Map::new();
    let mut env_vars = Vec::new();

    for (key, value) in env {
        let env_value = json_string(value).unwrap_or_else(|| value.to_string());
        if parse_env_placeholder(&env_value).as_deref() == Some(key.as_str()) {
            env_vars.push(TomlValue::String(key.clone()));
        } else if contains_env_placeholder(&env_value) {
            return None;
        } else {
            static_env.insert(key.clone(), TomlValue::String(env_value));
        }
    }

    if !env_vars.is_empty() {
        table.insert("env_vars".to_string(), TomlValue::Array(env_vars));
    }
    if !static_env.is_empty() {
        table.insert("env".to_string(), TomlValue::Table(static_env));
    }
    Some(())
}

fn parse_env_placeholder(value: &str) -> Option<String> {
    let name = value.strip_prefix("${")?.strip_suffix('}')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some(name.to_string())
}

fn contains_env_placeholder(value: &str) -> bool {
    value.contains("${")
}

fn json_string_vec(value: &JsonValue) -> Vec<String> {
    match value {
        JsonValue::Array(values) => values.iter().filter_map(json_string).collect(),
        _ => json_string(value).into_iter().collect(),
    }
}

fn json_string(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::Null => None,
        JsonValue::String(value) => Some(value.clone()),
        JsonValue::Bool(value) => Some(value.to_string()),
        JsonValue::Number(value) => Some(value.to_string()),
        JsonValue::Array(_) | JsonValue::Object(_) => None,
    }
}


const EXTERNAL_AGENT_KNOWN_MARKETPLACES_PATH: &str = "plugins/known_marketplaces.json";
const EXTERNAL_OFFICIAL_MARKETPLACE_NAME: &str = "claude-plugins-official";
const EXTERNAL_OFFICIAL_MARKETPLACE_SOURCE: &str = "anthropics/claude-plugins-official";

pub(super) fn plugin_sources(home: &Path, item: &ExternalAgentConfigMigrationItem) -> io::Result<BTreeMap<String, MarketplaceImportSource>> {
    let cwd = item.cwd.as_deref().filter(|cwd| !cwd.as_os_str().is_empty()).map(repo_root).transpose()?.flatten();
    if item.cwd.as_ref().is_some_and(|cwd| !cwd.as_os_str().is_empty()) && cwd.is_none() {
        return Err(invalid_data_error("plugin import working directory does not exist"));
    }
    let paths = Paths::new(home, cwd.as_deref());
    let settings = settings(&paths.source)?.unwrap_or_else(|| JsonValue::Object(Default::default()));
    Ok(marketplace_import_sources(&settings, cwd.as_deref().unwrap_or(&paths.external_home), &paths.external_home))
}

fn has_enabled_plugin_for_marketplace(settings: &JsonValue, marketplace: &str) -> bool {
    settings.get("enabledPlugins").and_then(JsonValue::as_object).is_some_and(|plugins|
        plugins.iter().any(|(name, enabled)| enabled.as_bool() == Some(true) && name.rsplit_once('@').is_some_and(|(_, source)| source == marketplace)))
}

fn marketplace_import_sources(
        settings: &JsonValue,
        source_root: &Path,
        external_agent_home: &PathBuf,
    ) -> BTreeMap<String, MarketplaceImportSource> {
        let known_marketplaces_path = external_agent_home
            .join(EXTERNAL_AGENT_KNOWN_MARKETPLACES_PATH);
        let known_marketplaces = match read_json(&known_marketplaces_path) {
            Ok(known_marketplaces) => known_marketplaces,
            Err(err) => {
                tracing::warn!(
                    path = %known_marketplaces_path.display(),
                    error = %err,
                    "ignoring invalid external agent marketplace registry"
                );
                None
            }
        };
        let mut import_sources = known_marketplaces
            .as_ref()
            .map(|known_marketplaces| {
                collect_marketplace_import_sources(
                    known_marketplaces,
                    external_agent_home.as_path(),
                )
            })
            .unwrap_or_default();

        if let Some(extra_known_marketplaces) = settings
            .as_object()
            .and_then(|settings| settings.get("extraKnownMarketplaces"))
        {
            let mut scoped_marketplaces = extra_known_marketplaces.clone();
            if let Some(scoped_marketplaces) = scoped_marketplaces.as_object_mut() {
                for (name, scoped_marketplace) in scoped_marketplaces {
                    import_sources.remove(name);
                    let Some(known_marketplace) = known_marketplaces
                        .as_ref()
                        .and_then(JsonValue::as_object)
                        .and_then(|known_marketplaces| known_marketplaces.get(name))
                    else {
                        continue;
                    };
                    if scoped_marketplace.get("source") != known_marketplace.get("source") {
                        continue;
                    }
                    let Some(install_location) = known_marketplace
                        .get("installLocation")
                        .and_then(JsonValue::as_str)
                    else {
                        continue;
                    };
                    let install_location = Path::new(install_location);
                    let install_location = if install_location.is_absolute() {
                        install_location.to_path_buf()
                    } else {
                        external_agent_home.join(install_location)
                    };
                    let Some(scoped_marketplace) = scoped_marketplace.as_object_mut() else {
                        continue;
                    };
                    scoped_marketplace.insert(
                        "installLocation".to_string(),
                        JsonValue::String(install_location.display().to_string()),
                    );
                }
            }
            import_sources.extend(collect_marketplace_import_sources(
                &scoped_marketplaces,
                source_root,
            ));
        }

        if has_enabled_plugin_for_marketplace(settings, EXTERNAL_OFFICIAL_MARKETPLACE_NAME)
            && !import_sources.contains_key(EXTERNAL_OFFICIAL_MARKETPLACE_NAME)
        {
            import_sources.insert(
                EXTERNAL_OFFICIAL_MARKETPLACE_NAME.to_string(),
                MarketplaceImportSource {
                    source: EXTERNAL_OFFICIAL_MARKETPLACE_SOURCE.to_string(),
                    ref_name: None,
                },
            );
        }

        import_sources
    }

fn collect_marketplace_import_sources(
    marketplaces: &JsonValue,
    source_root: &Path,
) -> BTreeMap<String, MarketplaceImportSource> {
    marketplaces
        .as_object()
        .map(|extra_known_marketplaces| {
            extra_known_marketplaces
                .iter()
                .filter_map(|(name, value)| {
                    let source_fields = if let Some(source) = value.get("source")
                        && source.is_object()
                    {
                        source.as_object()?
                    } else {
                        value.as_object()?
                    };
                    let source_kind = source_fields
                        .get("source")
                        .and_then(JsonValue::as_str)
                        .map(str::trim);
                    let declared_source = match source_kind {
                        Some("github") => source_fields.get("repo"),
                        Some("git") => source_fields.get("url"),
                        Some("directory" | "local") => source_fields.get("path"),
                        Some("file" | "url" | "npm" | "settings") => None,
                        Some(_) => source_fields.get("source"),
                        None => source_fields
                            .get("repo")
                            .or_else(|| source_fields.get("url"))
                            .or_else(|| source_fields.get("path"))
                            .or_else(|| value.get("source")),
                    }
                    .and_then(JsonValue::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                    let materialized_source = value
                        .get("installLocation")
                        .and_then(JsonValue::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .and_then(|value| {
                            let path = Path::new(value);
                            let path = if path.is_absolute() {
                                path.to_path_buf()
                            } else {
                                source_root.join(path)
                            };
                            path.is_dir().then(|| path.display().to_string())
                        });
                    let (source, ref_name) = if let Some(source) = declared_source {
                        let source = if matches!(source_kind, Some("directory" | "local")) {
                            let path = Path::new(source);
                            if path.is_absolute() {
                                path.to_path_buf()
                            } else {
                                source_root.join(path)
                            }
                            .display()
                            .to_string()
                        } else {
                            resolve_external_marketplace_source(source, source_root)
                        };
                        let ref_name = source_fields
                            .get("ref")
                            .or_else(|| value.get("ref"))
                            .and_then(JsonValue::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(ToOwned::to_owned);
                        (source, ref_name)
                    } else {
                        (materialized_source?, None)
                    };

                    Some((name.clone(), MarketplaceImportSource { source, ref_name }))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MarketplaceImportSource {
    pub source: String,
    pub ref_name: Option<String>,
}

fn resolve_external_marketplace_source(source: &str, source_root: &Path) -> String {
    if !looks_like_relative_local_path(source) {
        return source.to_string();
    }

    source_root.join(source).display().to_string()
}

fn looks_like_relative_local_path(source: &str) -> bool {
    source.starts_with("./") || source.starts_with("../") || source == "." || source == ".."
}



const CONFIG_CR_MARKER: &str = "\0codex-import-cr\0";

fn parse_config_document(serialized: &str, raw_values: &mut Vec<String>) -> io::Result<DocumentMut> {
    let original = toml_edit::Document::parse(serialized)
        .map_err(|err| invalid_data_error(format!("invalid existing config.toml: {err}")))?;
    let mut edited = original.clone().into_mut();
    preserve_config_table(original.as_table(), edited.as_table_mut(), serialized, raw_values, true)?;
    let trailing = protect_config_raw(edited.trailing().as_str().unwrap_or_default());
    edited.set_trailing(trailing);
    Ok(edited)
}

// toml_edit normalizes CRs even in unchanged raw representations. Its existing
// document/decor machinery still owns the edit; temporary NUL-delimited markers
// protect existing raw text until rendering finishes. Valid TOML cannot contain
// these literal NULs, and no marked/intermediate document is ever published.
fn protect_config_raw(raw: &str) -> String {
    raw.replace('\r', CONFIG_CR_MARKER)
}

fn protect_config_decor(decor: &mut toml_edit::Decor) {
    let prefix = decor.prefix().and_then(|raw| raw.as_str()).map(protect_config_raw);
    let suffix = decor.suffix().and_then(|raw| raw.as_str()).map(protect_config_raw);
    if let Some(prefix) = prefix { decor.set_prefix(prefix); }
    if let Some(suffix) = suffix { decor.set_suffix(suffix); }
}

fn protect_config_keys(table: &mut dyn toml_edit::TableLike) {
    let keys = table.iter().map(|(key, _)| key.to_string()).collect::<Vec<_>>();
    for key in keys {
        if let Some(mut key) = table.key_mut(&key) {
            protect_config_decor(key.leaf_decor_mut());
            protect_config_decor(key.dotted_decor_mut());
        }
    }
}

fn append_config_crlf(decor: &mut toml_edit::Decor) {
    let mut suffix = decor.suffix().and_then(|raw| raw.as_str()).unwrap_or_default().to_string();
    suffix.push_str(CONFIG_CR_MARKER);
    decor.set_suffix(suffix);
}

fn preserve_config_table(
    original: &toml_edit::Table,
    edited: &mut toml_edit::Table,
    serialized: &str,
    raw_values: &mut Vec<String>,
    root: bool,
) -> io::Result<()> {
    protect_config_decor(edited.decor_mut());
    protect_config_keys(edited);
    if !root && !original.is_implicit() && !original.is_dotted()
        && original.span().is_some_and(|span| crlf_after(serialized, span.start))
    {
        append_config_crlf(edited.decor_mut());
    }
    for (key, value) in original.iter() {
        let updated = edited.get_mut(key).ok_or_else(|| invalid_data_error("parsed config key disappeared"))?;
        match (value, updated) {
            (TomlItem::Table(original), TomlItem::Table(updated)) => {
                preserve_config_table(original, updated, serialized, raw_values, false)?;
            }
            (TomlItem::ArrayOfTables(original), TomlItem::ArrayOfTables(updated)) => {
                for (original, updated) in original.iter().zip(updated.iter_mut()) {
                    preserve_config_table(original, updated, serialized, raw_values, false)?;
                }
            }
            (TomlItem::Value(original), TomlItem::Value(updated)) => {
                preserve_config_value(original, updated, serialized, raw_values, true)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn preserve_config_value(
    original: &toml_edit::Value,
    edited: &mut toml_edit::Value,
    serialized: &str,
    raw_values: &mut Vec<String>,
    assignment: bool,
) -> io::Result<()> {
    protect_config_decor(edited.decor_mut());
    let span = original.span().ok_or_else(|| invalid_data_error("parsed config value has no source span"))?;
    if let (Some(original), Some(updated)) = (original.as_inline_table(), edited.as_inline_table_mut()) {
        protect_config_keys(updated);
        let trailing = protect_config_raw(updated.trailing().as_str().unwrap_or_default());
        updated.set_trailing(trailing);
        for (key, value) in original.iter() {
            let updated = updated.get_mut(key).ok_or_else(|| invalid_data_error("parsed inline config key disappeared"))?;
            preserve_config_value(value, updated, serialized, raw_values, false)?;
        }
    } else {
        let raw = serialized.get(span.clone()).ok_or_else(|| invalid_data_error("parsed config value span is invalid"))?;
        let index = raw_values.len();
        raw_values.push(raw.to_string());
        let decor = edited.decor_mut();
        let prefix = decor.prefix().and_then(|raw| raw.as_str()).unwrap_or_default();
        let prefix = format!("{prefix}\0codex-import-value:{index}:start\0");
        let suffix = decor.suffix().and_then(|raw| raw.as_str()).unwrap_or_default();
        let suffix = format!("\0codex-import-value:{index}:end\0{suffix}");
        decor.set_prefix(prefix);
        decor.set_suffix(suffix);
    }
    if assignment && crlf_after(serialized, span.end) {
        append_config_crlf(edited.decor_mut());
    }
    Ok(())
}

fn crlf_after(serialized: &str, offset: usize) -> bool {
    serialized.get(offset..).and_then(|tail| tail.find('\n'))
        .is_some_and(|newline| offset + newline > 0 && serialized.as_bytes()[offset + newline - 1] == b'\r')
}

fn default_crlf(serialized: &str) -> bool {
    crlf_after(serialized, 0)
}


fn clear_imported_table_positions(item: &mut TomlItem) {
    match item {
        TomlItem::Table(table) => {
            table.set_position(None);
            for (_, item) in table.iter_mut() { clear_imported_table_positions(item); }
        }
        TomlItem::ArrayOfTables(tables) => {
            for table in tables.iter_mut() {
                table.set_position(None);
                for (_, item) in table.iter_mut() { clear_imported_table_positions(item); }
            }
        }
        _ => {}
    }
}

fn merge_missing_document(existing: &mut TomlItem, incoming: &TomlItem, recursive: bool) -> io::Result<bool> {
    let inline = existing.is_inline_table();
    let existing = existing.as_table_like_mut().ok_or_else(|| invalid_data_error("config must be a TOML table"))?;
    let incoming = incoming.as_table_like().ok_or_else(|| invalid_data_error("migration must be a TOML table"))?;
    let mut changed = false;
    for (key, value) in incoming.iter() {
        if let Some(current) = existing.get_mut(key) {
            if recursive && current.as_table_like().is_some() && value.as_table_like().is_some() {
                changed |= merge_missing_document(current, value, true)?;
            }
        } else {
            let value = if inline {
                let mut value = value.clone().into_value().map_err(|_| invalid_data_error("imported inline value is not representable"))?;
                // Assignment EOL decor is inappropriate inside an inline table.
                value.decor_mut().clear();
                TomlItem::Value(value)
            } else { value.clone() };
            existing.insert(key, value);
            changed = true;
        }
    }
    Ok(changed)
}

fn render_config_document(original: &str, edited: &DocumentMut, raw_values: &[String]) -> io::Result<String> {
    let marked = edited.to_string();
    let mut rendered = String::with_capacity(marked.len());
    let mut remaining = marked.as_str();
    while let Some(start) = remaining.find('\0') {
        rendered.push_str(&remaining[..start]);
        let marker = &remaining[start + 1..];
        let end = marker.find('\0').ok_or_else(|| invalid_data_error("incomplete config preservation marker"))?;
        let tag = &marker[..end];
        remaining = &marker[end + 1..];
        if tag == "codex-import-cr" {
            rendered.push('\r');
        } else if let Some(index) = tag.strip_prefix("codex-import-value:").and_then(|tag| tag.strip_suffix(":start")) {
            let index = index.parse::<usize>().map_err(|_| invalid_data_error("invalid config preservation marker"))?;
            let raw = raw_values.get(index).ok_or_else(|| invalid_data_error("missing original config value"))?;
            let close = format!("\0codex-import-value:{index}:end\0");
            let end = remaining.find(&close).ok_or_else(|| invalid_data_error("incomplete original config value"))?;
            rendered.push_str(raw);
            remaining = &remaining[end + close.len()..];
        } else {
            return Err(invalid_data_error("unexpected config preservation marker"));
        }
    }
    rendered.push_str(remaining);
    if !original.is_empty() && !original.ends_with('\n') {
        if rendered.ends_with("\r\n") { rendered.truncate(rendered.len() - 2); }
        else if rendered.ends_with('\n') { rendered.pop(); }
    }
    Ok(rendered)
}

