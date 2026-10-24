//! Format adapters for retained hooks, role files, and skills.
use codex_hooks::{HookEventName, hook_event_supports_matcher};
use noyalib::compat::serde_yaml;
use noyalib::compat::serde_yaml::Value as YamlValue;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;
use toml::Value as TomlValue;
use super::files::rewrite_terms as rewrite_external_agent_terms;

const EXTERNAL_AGENT_HOOKS_SUBDIR: &str = "hooks";
const EXTERNAL_AGENT_MIGRATED_HOOKS_SUBDIR: &str = "hooks";
const COMMAND_SKILL_PREFIX: &str = "source-command";
const MAX_SKILL_NAME_LEN: usize = 64;

pub(super) struct DocumentPlan {
    pub name: String,
    pub target: PathBuf,
    pub contents: String,
}

pub(super) struct DocumentConversions {
    pub plans: Vec<DocumentPlan>,
    pub failures: Vec<(PathBuf, String)>,
}

pub(super) fn documents(source: &Path, target: &Path, agents: bool) -> io::Result<DocumentConversions> {
    let mut plans: Vec<DocumentPlan> = Vec::new();
    let mut failures = Vec::new();
    if agents {
        let mut role_names = std::collections::HashSet::new();
        for file in agent_source_files(source)? {
            let target = subagent_target_file(&file, target).ok_or_else(|| invalid_data_error("invalid subagent filename"))?;
            if target.try_exists()? { continue; }
            let document = parse_document(&file)?;
            let Some(metadata) = agent_metadata(&document) else {
                failures.push((file, "subagent needs name/description/body and cannot use tool restrictions or an unsupported permission mode".to_string()));
                continue;
            };
            if metadata.effort.as_ref().is_some_and(|effort| map_agent_reasoning_effort(effort).is_none()) {
                failures.push((file, "subagent has an unsupported reasoning effort".to_string()));
                continue;
            }
            if document.frontmatter.keys().any(|key| !matches!(key.as_str(), "name" | "description" | "permissionMode" | "effort")) {
                failures.push((file, "subagent has runtime controls that cannot be represented by a Codex role".to_string()));
                continue;
            }
            if !role_names.insert(metadata.name.clone()) {
                plans.retain(|plan| plan.name != metadata.name);
                failures.push((file, format!("source subagent name {:?} is duplicated", metadata.name)));
                continue;
            }
            plans.push(DocumentPlan { name: metadata.name.clone(), target, contents: render_agent_toml(&document.body, &metadata)? });
        }
    } else {
        let mut names = std::collections::HashSet::new();
        for file in command_source_files(source)? {
            if file.file_stem().and_then(|stem| stem.to_str()) == Some("README") { continue; }
            let document = parse_document(&file)?;
            let Some(name) = command_skill_name_if_supported(source, &file, &document) else {
                failures.push((file, "command cannot be represented as a skill: description required; runtime controls, argument/template expansion, shell interpolation and file references are unsupported".to_string()));
                continue;
            };
            if !names.insert(name) { failures.push((file, "source command names collide after skill-name normalization".to_string())); }
        }
        for (file, name, document) in unique_supported_command_sources(source)? {
            let destination = target.join(&name).join("SKILL.md");
            if !destination.try_exists()? {
                let source_name = command_source_name(source, &file);
                let description = command_skill_description(&document, &source_name).ok_or_else(|| invalid_data_error("command description missing"))?;
                plans.push(DocumentPlan { target: destination, contents: render_command_skill(&document.body, &name, &description, &source_name), name });
            }
        }
    }
    Ok(DocumentConversions { plans, failures })
}

pub(super) fn hooks(source: &Path, target_config_dir: &Path) -> io::Result<(Vec<String>, String, Vec<(PathBuf, String)>)> {
    let (migration, failures) = hook_migration(source, Some(target_config_dir))?;
    let names = migration.keys().cloned().collect();
    let mut payload = serde_json::Map::new();
    payload.insert("hooks".to_string(), JsonValue::Object(migration));
    let rendered = serde_json::to_string_pretty(&payload).map_err(io::Error::other)?;
    Ok((names, format!("{rendered}\n"), failures))
}

fn invalid_data_error(message: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message.into()) }
fn external_agent_config_dir() -> String { ".claude".to_string() }
fn external_agent_project_dir_env_var() -> String { "CLAUDE_PROJECT_DIR".to_string() }


#[derive(Debug)]
struct ParsedDocument {
    frontmatter: BTreeMap<String, FrontmatterValue>,
    body: String,
    frontmatter_error: Option<String>,
}

#[derive(Debug)]
enum FrontmatterValue {
    Scalar(String),
    Other,
}

#[derive(Debug)]
struct AgentMetadata {
    name: String,
    description: String,
    permission_mode: Option<String>,
    effort: Option<String>,
}
fn hook_migration(
    source_external_agent_dir: &Path,
    target_config_dir: Option<&Path>,
) -> io::Result<(serde_json::Map<String, JsonValue>, Vec<(PathBuf, String)>)> {
    let mut settings_files = Vec::new();
    let mut disabled = false;
    for name in ["settings.json", "settings.local.json"] {
        let path = source_external_agent_dir.join(name);
        if !path.is_file() { continue; }
        let settings: JsonValue = serde_json::from_str(&fs::read_to_string(&path)?)
            .map_err(|err| invalid_data_error(format!("invalid hooks settings: {err}")))?;
        if let Some(value) = settings.get("disableAllHooks").and_then(JsonValue::as_bool) { disabled = value; }
        settings_files.push((path, settings));
    }
    if disabled { return Ok((Default::default(), Vec::new())); }
    let mut migration = serde_json::Map::new();
    let mut failures = Vec::new();
    for (path, settings) in settings_files {
        let Some(config) = settings.get("hooks") else { continue; };
        let Some(config) = config.as_object() else {
            failures.push((path, "hooks must be an object".to_string()));
            continue;
        };
        for (event, groups) in config {
            let Some(groups) = groups.as_array() else {
                failures.push((path.clone(), format!("hook event {event:?} must contain an array")));
                continue;
            };
            for group in groups {
                let selections = if let Some(hooks) = group.get("hooks").and_then(JsonValue::as_array) {
                    if hooks.is_empty() { vec![group.clone()] } else {
                        hooks.iter().map(|hook| {
                            let mut selection = group.clone();
                            if let Some(object) = selection.as_object_mut() { object.insert("hooks".to_string(), JsonValue::Array(vec![hook.clone()])); }
                            selection
                        }).collect()
                    }
                } else { vec![group.clone()] };
                for selection in selections {
                    let mut events = serde_json::Map::new();
                    events.insert(event.clone(), JsonValue::Array(vec![selection]));
                    let mut selected = serde_json::Map::new();
                    selected.insert("hooks".to_string(), JsonValue::Object(events));
                    if let Err(err) = append_convertible_hook_groups(&JsonValue::Object(selected), &mut migration, target_config_dir, source_external_agent_dir) {
                        failures.push((path.clone(), err.to_string()));
                    }
                }
            }
        }
    }
    Ok((migration, failures))
}

fn append_convertible_hook_groups(
    settings: &JsonValue,
    hooks_payload: &mut serde_json::Map<String, JsonValue>,
    target_config_dir: Option<&Path>,
    source_external_agent_dir: &Path,
) -> io::Result<()> {
    let Some(config) = settings.get("hooks") else { return Ok(()); };
    let config = config.as_object().ok_or_else(|| invalid_data_error("hooks must be an object"))?;
    for (event_name, groups) in config {
        let event = HookEventName::all().into_iter().find(|event| event.as_pascal_case_label() == event_name.as_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, format!("hook event {event_name:?} is not supported by the current hooks owner")))?;
        let groups = groups.as_array().ok_or_else(|| invalid_data_error(format!("hook event {event_name:?} must contain an array")))?;
        for group in groups {
            let group = group.as_object().ok_or_else(|| invalid_data_error("hook group must be an object"))?;
            if let Some(key) = group.keys().find(|key| !matches!(key.as_str(), "matcher" | "hooks")) {
                return Err(io::Error::new(io::ErrorKind::Unsupported, format!("hook event {event_name:?} uses unsupported group control {key:?}")));
            }
            let mut converted = Vec::new();
            let hooks = group.get("hooks").and_then(JsonValue::as_array).ok_or_else(|| invalid_data_error("hook group requires a hooks array"))?;
            for hook in hooks {
                let hook = hook.as_object().ok_or_else(|| invalid_data_error("hook must be an object"))?;
                if hook.get("type").and_then(JsonValue::as_str).unwrap_or("command") != "command" {
                    return Err(io::Error::new(io::ErrorKind::Unsupported, format!("hook event {event_name:?} uses a non-command handler")));
                }
                if let Some(key) = hook.keys().find(|key| !matches!(key.as_str(), "type" | "command" | "timeout" | "timeoutSec" | "statusMessage" | "async")) {
                    return Err(io::Error::new(io::ErrorKind::Unsupported, format!("hook event {event_name:?} uses unsupported handler control {key:?}")));
                }
                if let Some(value) = hook.get("async") {
                    if value.as_bool() != Some(false) {
                        return Err(io::Error::new(io::ErrorKind::Unsupported, format!("hook event {event_name:?} requires asynchronous handler semantics")));
                    }
                }
                let command = hook.get("command").and_then(JsonValue::as_str).map(str::trim).filter(|command| !command.is_empty())
                    .ok_or_else(|| invalid_data_error("command hook requires a non-empty command"))?;
                let command = rewrite_hook_command(command, target_config_dir, source_external_agent_dir)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, format!("hook command cannot be safely relocated: {command}")))?;
                let mut handler = serde_json::Map::new();
                handler.insert("type".to_string(), JsonValue::String("command".to_string()));
                handler.insert("command".to_string(), JsonValue::String(command));
                if let Some(timeout) = hook.get("timeout").or_else(|| hook.get("timeoutSec")) {
                    let timeout = json_u64(timeout).ok_or_else(|| invalid_data_error("hook timeout must be an unsigned integer"))?;
                    handler.insert("timeout".to_string(), JsonValue::from(timeout));
                }
                if let Some(status) = hook.get("statusMessage") {
                    let status = status.as_str().ok_or_else(|| invalid_data_error("hook statusMessage must be a string"))?;
                    handler.insert("statusMessage".to_string(), JsonValue::String(rewrite_external_agent_terms(status)));
                }
                converted.push(JsonValue::Object(handler));
            }
            if converted.is_empty() { continue; }
            let mut converted_group = serde_json::Map::new();
            if let Some(matcher) = group.get("matcher") {
                let matcher = matcher.as_str().ok_or_else(|| invalid_data_error("hook matcher must be a string"))?;
                if !hook_event_supports_matcher(event) && !matcher.is_empty() {
                    return Err(io::Error::new(io::ErrorKind::Unsupported, format!("hook event {event_name:?} cannot preserve a matcher")));
                }
                if hook_event_supports_matcher(event) {
                    converted_group.insert("matcher".to_string(), JsonValue::String(matcher.to_string()));
                }
            }
            converted_group.insert("hooks".to_string(), JsonValue::Array(converted));
            hooks_payload.entry(event_name.clone()).or_insert_with(|| JsonValue::Array(Vec::new()))
                .as_array_mut().ok_or_else(|| invalid_data_error("hook output group must be an array"))?.push(JsonValue::Object(converted_group));
        }
    }
    Ok(())
}

// Relocate only complete static path tokens. Ambiguous source references cannot
// safely be activated against a different hook tree.
fn rewrite_hook_command(
    command: &str,
    target_config_dir: Option<&Path>,
    source_external_agent_dir: &Path,
) -> Option<String> {
    let Some(target_config_dir) = target_config_dir else {
        return Some(command.to_string());
    };
    if looks_like_windows_hook_command(command) {
        return None;
    }
    let relative_root = format!(
        "{}/{EXTERNAL_AGENT_HOOKS_SUBDIR}/",
        external_agent_config_dir()
    );
    let absolute_root = format!(
        "{}/",
        source_external_agent_dir
            .join(EXTERNAL_AGENT_HOOKS_SUBDIR)
            .to_string_lossy()
            .replace('\\', "/")
    );
    let target_root = target_config_dir.join(EXTERNAL_AGENT_MIGRATED_HOOKS_SUBDIR);
    let mut output = String::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut ranges = Vec::new();
    for (index, ch) in command.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
        } else if quote.is_none()
            && (ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '<' | '>' | '(' | ')'))
        {
            if start < index {
                ranges.push((start, index));
            }
            start = index + ch.len_utf8();
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    if start < command.len() {
        ranges.push((start, command.len()));
    }
    let mut copied = 0;
    for (start, end) in ranges {
        let token = &command[start..end];
        if !token.contains(&relative_root) && !token.contains(&absolute_root) {
            continue;
        }
        let path = token
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .or_else(|| token.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
            .unwrap_or(token);
        let suffix = path
            .strip_prefix(&absolute_root)
            .or_else(|| path.strip_prefix(&relative_root))
            .or_else(|| {
                path.strip_prefix("./")
                    .and_then(|s| s.strip_prefix(&relative_root))
            })?;
        if suffix.is_empty()
            || suffix.chars().any(|ch| {
                matches!(
                    ch,
                    '\\' | '\'' | '"' | '$' | '`' | '*' | '?' | '[' | '{' | '}' | ':'
                )
            })
            || suffix
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
        {
            return None;
        }
        output.push_str(&command[copied..start]);
        output.push_str(&shell_single_quote(
            &target_root.join(suffix).to_string_lossy(),
        ));
        copied = end;
    }
    output.push_str(&command[copied..]);
    Some(output)
}

fn looks_like_windows_hook_command(command: &str) -> bool {
    let source_hooks_backslash_path = format!(
        r"{}\{EXTERNAL_AGENT_HOOKS_SUBDIR}\",
        external_agent_config_dir()
    );
    let project_dir_env_var = external_agent_project_dir_env_var();
    command.contains(&source_hooks_backslash_path)
        || command.contains(&format!("%{project_dir_env_var}%"))
        || command.contains(&format!("$env:{project_dir_env_var}"))
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn copy_hook_scripts(source_external_agent_dir: &Path, target_config_dir: &Path, cancellation: &CancellationToken) -> io::Result<()> {
    let source_hooks = source_external_agent_dir.join(EXTERNAL_AGENT_HOOKS_SUBDIR);
    if !source_hooks.is_dir() {
        return Ok(());
    }
    let target_hooks = target_config_dir.join(EXTERNAL_AGENT_MIGRATED_HOOKS_SUBDIR);
    copy_dir_recursive_skip_existing(&source_hooks, &target_hooks, cancellation)
}

fn copy_dir_recursive_skip_existing(source: &Path, target: &Path, cancellation: &CancellationToken) -> io::Result<()> {
    super::files::check_cancel(cancellation)?;
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        super::files::check_cancel(cancellation)?;
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive_skip_existing(&source_path, &target_path, cancellation)?;
        } else if file_type.is_file() {
            let contents = fs::read(&source_path)?;
            if !publish_new_file(
                &target_path,
                &contents,
                Some(fs::metadata(&source_path)?.permissions()),
            )? && fs::read(&target_path)? != contents
            {
                return Err(invalid_data_error(format!(
                    "conflicting hook script: {}",
                    target_path.display()
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn publish_new_file(
    path: &Path,
    contents: &[u8],
    permissions: Option<fs::Permissions>,
) -> io::Result<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data_error("target has no parent"))?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    if let Some(permissions) = permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.error),
    }
}

fn agent_source_files(source_agents: &Path) -> io::Result<Vec<PathBuf>> {
    if !source_agents.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(source_agents)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file()
            || path.extension().and_then(|ext| ext.to_str()) != Some("md")
        {
            continue;
        }
        if path.file_stem().and_then(|stem| stem.to_str()) == Some("README") {
            continue;
        }
        files.push(path);
    }
    files.sort();
    Ok(files)
}

fn subagent_target_file(source_file: &Path, target_agents: &Path) -> Option<PathBuf> {
    Some(target_agents.join(format!("{}.toml", source_file.file_stem()?.to_str()?)))
}

fn command_source_files(source_commands: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_markdown_files(source_commands, &mut files)?;
    files.sort();
    Ok(files)
}

fn unique_supported_command_sources(
    source_commands: &Path,
) -> io::Result<Vec<(PathBuf, String, ParsedDocument)>> {
    let mut by_name = BTreeMap::<String, Vec<(PathBuf, ParsedDocument)>>::new();
    for source_file in command_source_files(source_commands)? {
        let document = parse_document(&source_file)?;
        let Some(name) = command_skill_name_if_supported(source_commands, &source_file, &document)
        else {
            continue;
        };
        by_name
            .entry(name)
            .or_default()
            .push((source_file, document));
    }

    Ok(by_name
        .into_iter()
        .filter_map(|(name, mut source_files)| {
            if source_files.len() != 1 {
                return None;
            }
            let (source_file, document) = source_files.pop()?;
            Some((source_file, name, document))
        })
        .collect())
}

fn collect_markdown_files(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_markdown_files(&path, files)?;
        } else if file_type.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("md")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn parse_document(source_file: &Path) -> io::Result<ParsedDocument> {
    let content = fs::read_to_string(source_file)?;
    Ok(parse_document_content(&content))
}

fn parse_document_content(content: &str) -> ParsedDocument {
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return ParsedDocument {
            frontmatter: BTreeMap::new(),
            body: content.to_string(),
            frontmatter_error: None,
        };
    };
    let Some((end, body_start)) = frontmatter_end(rest) else {
        return ParsedDocument {
            frontmatter: BTreeMap::new(),
            body: content.to_string(),
            frontmatter_error: None,
        };
    };

    let raw_frontmatter = &rest[..end];
    let body = &rest[body_start..];
    let (frontmatter, frontmatter_error) = parse_frontmatter(raw_frontmatter);
    ParsedDocument {
        frontmatter,
        body: body.to_string(),
        frontmatter_error,
    }
}

fn frontmatter_end(rest: &str) -> Option<(usize, usize)> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some((offset, offset + line.len()));
        }
        offset += line.len();
    }
    None
}

fn parse_frontmatter(
    raw_frontmatter: &str,
) -> (BTreeMap<String, FrontmatterValue>, Option<String>) {
    let parsed: YamlValue = match serde_yaml::from_str(raw_frontmatter) {
        Ok(parsed) => parsed,
        Err(err) => return (BTreeMap::new(), Some(err.to_string())),
    };
    let Some(mapping) = parsed.as_mapping() else {
        return (
            BTreeMap::new(),
            Some("frontmatter is not a YAML mapping".to_string()),
        );
    };

    let mut frontmatter = BTreeMap::new();
    for (key, value) in mapping {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        frontmatter.insert(key.to_string(), frontmatter_value_from_yaml(value));
    }

    (frontmatter, None)
}

fn frontmatter_value_from_yaml(value: &YamlValue) -> FrontmatterValue {
    match value {
        YamlValue::String(value) => FrontmatterValue::Scalar(value.trim().to_string()),
        YamlValue::Bool(value) => FrontmatterValue::Scalar(value.to_string()),
        YamlValue::Number(value) => FrontmatterValue::Scalar(value.to_string()),
        YamlValue::Null | YamlValue::Sequence(_) | YamlValue::Mapping(_) | YamlValue::Tagged(_) => {
            FrontmatterValue::Other
        }
    }
}

fn agent_metadata(document: &ParsedDocument) -> Option<AgentMetadata> {
    if document.frontmatter_error.is_some()
        || document.body.trim().is_empty()
        || ["tools", "disallowedTools"]
            .iter()
            .any(|key| document.frontmatter.contains_key(*key))
        || document
            .frontmatter
            .get("permissionMode")
            .is_some_and(|value| {
                value
                    .as_scalar()
                    .and_then(map_agent_permission_mode)
                    .is_none()
            })
    {
        return None;
    }
    let name = document
        .frontmatter
        .get("name")
        .and_then(FrontmatterValue::as_scalar)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)?;

    let description = document
        .frontmatter
        .get("description")
        .and_then(FrontmatterValue::as_scalar)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)?;

    Some(AgentMetadata {
        name,
        description,
        permission_mode: frontmatter_string(&document.frontmatter, "permissionMode"),
        effort: frontmatter_string(&document.frontmatter, "effort"),
    })
}

fn render_agent_toml(body: &str, metadata: &AgentMetadata) -> io::Result<String> {
    let mut document = toml::map::Map::new();
    document.insert("name".to_string(), TomlValue::String(metadata.name.clone()));
    document.insert(
        "description".to_string(),
        TomlValue::String(rewrite_external_agent_terms(&metadata.description)),
    );
    if let Some(effort) = metadata.effort.as_ref()
        && let Some(effort) = map_agent_reasoning_effort(effort)
    {
        document.insert(
            "model_reasoning_effort".to_string(),
            TomlValue::String(effort),
        );
    }
    if let Some(sandbox_mode) = metadata
        .permission_mode
        .as_deref()
        .and_then(map_agent_permission_mode)
    {
        document.insert(
            "sandbox_mode".to_string(),
            TomlValue::String(sandbox_mode.to_string()),
        );
    }
    document.insert(
        "developer_instructions".to_string(),
        TomlValue::String(render_agent_body(body)),
    );

    let serialized = toml::to_string_pretty(&TomlValue::Table(document))
        .map_err(|err| invalid_data_error(format!("failed to serialize agent TOML: {err}")))?;
    Ok(format!("{}\n", serialized.trim_end()))
}

fn render_agent_body(body: &str) -> String {
    let body = body.trim().to_string();
    if body.is_empty() {
        "No subagent instructions were found.".to_string()
    } else {
        body
    }
}

fn command_skill_name(source_commands: &Path, source_file: &Path) -> String {
    slugify_name(&format!(
        "{COMMAND_SKILL_PREFIX}-{}",
        command_source_name(source_commands, source_file)
    ))
}

fn command_skill_name_if_supported(
    source_commands: &Path,
    source_file: &Path,
    document: &ParsedDocument,
) -> Option<String> {
    if source_file.file_stem().and_then(|stem| stem.to_str()) == Some("README") {
        return None;
    }
    // A plain skill cannot preserve source runtime controls such as allowed-tools,
    // forked execution, or disabled model invocation. Do not silently drop them.
    if document.frontmatter_error.is_some()
        || document
            .frontmatter
            .keys()
            .any(|key| !matches!(key.as_str(), "name" | "description" | "argument-hint"))
    {
        return None;
    }
    let source_name = command_source_name(source_commands, source_file);
    command_skill_description(document, &source_name)?;
    let name = command_skill_name(source_commands, source_file);
    if name.len() > MAX_SKILL_NAME_LEN && name.chars().nth(MAX_SKILL_NAME_LEN).is_some() {
        return None;
    }
    if has_unsupported_command_template_features(&document.body) {
        return None;
    }
    Some(name)
}

fn command_skill_description(document: &ParsedDocument, _source_name: &str) -> Option<String> {
    document
        .frontmatter
        .get("description")
        .and_then(FrontmatterValue::as_scalar)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn command_source_name(source_commands: &Path, source_file: &Path) -> String {
    source_file
        .strip_prefix(source_commands)
        .unwrap_or(source_file)
        .with_extension("")
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("-")
}

fn render_command_skill(body: &str, name: &str, description: &str, source_name: &str) -> String {
    let body = body.trim().to_string();
    let template_body = if body.is_empty() {
        "No command template body was found.".to_string()
    } else {
        body
    };
    format!(
        "---\nname: {}\ndescription: {}\n---\n\n# {name}\n\nUse this skill when the user asks to run the migrated source command `{source_name}`.\n\n## Command Template\n\n{template_body}\n",
        yaml_string(name),
        yaml_string(&rewrite_external_agent_terms(description)),
    )
}

fn has_unsupported_command_template_features(template: &str) -> bool {
    template.contains("$ARGUMENTS")
        || contains_numbered_argument_placeholder(template)
        || (template.contains("{{") && template.contains("}}"))
        || template.contains("!`")
        || template.contains("! `")
        || template
            .split_whitespace()
            .any(|token| token.strip_prefix('@').is_some_and(|rest| !rest.is_empty()))
}

fn contains_numbered_argument_placeholder(template: &str) -> bool {
    let bytes = template.as_bytes();
    bytes
        .windows(2)
        .any(|window| window[0] == b'$' && window[1].is_ascii_digit())
}

fn frontmatter_string(
    frontmatter: &BTreeMap<String, FrontmatterValue>,
    key: &str,
) -> Option<String> {
    frontmatter
        .get(key)
        .and_then(FrontmatterValue::as_scalar)
        .map(ToOwned::to_owned)
}

fn map_agent_reasoning_effort(effort: &str) -> Option<String> {
    let mapped = match effort {
        "max" => "xhigh".to_string(),
        _ => effort.to_string(),
    };
    matches!(
        mapped.as_str(),
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
    )
    .then_some(mapped)
}

fn map_agent_permission_mode(permission_mode: &str) -> Option<&'static str> {
    match permission_mode {
        "acceptEdits" => Some("workspace-write"),
        "readOnly" => Some("read-only"),
        _ => None,
    }
}

fn json_u64(value: &JsonValue) -> Option<u64> {
    if value.is_boolean() || value.is_null() {
        return None;
    }
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

#[expect(
    clippy::expect_used,
    reason = "A string serialized into an in-memory JSON buffer has no fallible values or writer"
)]
fn yaml_string(value: &str) -> String {
    // JSON quoted scalars are valid YAML and preserve line breaks and control characters.
    serde_json::to_string(value).expect("string serializes")
}

fn slugify_name(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "migrated".to_string()
    } else {
        slug
    }
}

impl FrontmatterValue {
    fn as_scalar(&self) -> Option<&str> {
        match self {
            Self::Scalar(value) => Some(value),
            Self::Other => None,
        }
    }
}





