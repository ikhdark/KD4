use std::path::Path;

use codex_file_system::acquire_resolved_atomic_write_lock;
use codex_file_system::write_atomically;
use tokio::task;
use toml_edit::DocumentMut;
use toml_edit::Item as TomlItem;
use toml_edit::Table as TomlTable;
use toml_edit::value;

use crate::CONFIG_TOML_FILE;
use crate::read_or_create_config_document;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginConfigEdit {
    SetEnabled { plugin_key: String, enabled: bool },
    Clear { plugin_key: String },
}

pub async fn set_user_plugin_enabled(
    codex_home: &Path,
    plugin_key: String,
    enabled: bool,
) -> std::io::Result<()> {
    apply_user_plugin_config_edits(
        codex_home,
        vec![PluginConfigEdit::SetEnabled {
            plugin_key,
            enabled,
        }],
    )
    .await
}

pub async fn clear_user_plugin(codex_home: &Path, plugin_key: String) -> std::io::Result<()> {
    apply_user_plugin_config_edits(codex_home, vec![PluginConfigEdit::Clear { plugin_key }]).await
}

pub async fn apply_user_plugin_config_edits(
    codex_home: &Path,
    edits: Vec<PluginConfigEdit>,
) -> std::io::Result<()> {
    let codex_home = codex_home.to_path_buf();
    task::spawn_blocking(move || apply_user_plugin_config_edits_blocking(&codex_home, edits))
        .await
        .map_err(|err| std::io::Error::other(format!("config persistence task panicked: {err}")))?
}

/// Persist plugin edits from a blocking context, including callers that own a
/// larger filesystem transaction that must remain intact until this write ends.
pub fn apply_user_plugin_config_edits_blocking(
    codex_home: &Path,
    edits: Vec<PluginConfigEdit>,
) -> std::io::Result<()> {
    if edits.is_empty() {
        return Ok(());
    }

    let config_path = codex_home.join(CONFIG_TOML_FILE);
    let (write_paths, _lock) = acquire_resolved_atomic_write_lock(&config_path)?;
    let mut doc = read_or_create_config_document(write_paths.read_path.as_deref())?;
    let mut mutated = false;
    for edit in edits {
        mutated |= match edit {
            PluginConfigEdit::SetEnabled {
                plugin_key,
                enabled,
            } => set_plugin_enabled(&mut doc, &plugin_key, enabled)?,
            PluginConfigEdit::Clear { plugin_key } => clear_plugin(&mut doc, &plugin_key)?,
        };
    }
    if !mutated {
        return Ok(());
    }
    write_atomically(&write_paths.write_path, &doc.to_string())
}

fn set_plugin_enabled(
    doc: &mut DocumentMut,
    plugin_key: &str,
    enabled: bool,
) -> std::io::Result<bool> {
    let inline = doc.get("plugins").is_some_and(TomlItem::is_inline_table);
    let plugins = ensure_table_for_write(&mut doc["plugins"])?;
    let plugin = ensure_table_for_write(plugins.entry(plugin_key).or_insert_with(|| {
        if inline {
            TomlItem::Value(toml_edit::InlineTable::new().into())
        } else {
            TomlItem::Table(new_implicit_table())
        }
    }))?;
    let mut replacement = value(enabled);
    if let Some(existing) = plugin.get("enabled") {
        if existing.as_bool() == Some(enabled) {
            return Ok(false);
        }
        preserve_decor(existing, &mut replacement);
    }
    plugin.insert("enabled", replacement);
    Ok(true)
}

fn clear_plugin(doc: &mut DocumentMut, plugin_key: &str) -> std::io::Result<bool> {
    let root = doc.as_table_mut();
    let Some(plugins_item) = root.get_mut("plugins") else {
        return Ok(false);
    };
    let plugins = ensure_table_for_write(plugins_item)?;
    let Some(plugin) = plugins.get(plugin_key) else {
        return Ok(false);
    };
    if plugin.as_table_like().is_none() {
        return Err(invalid_plugin_table());
    }
    Ok(plugins.remove(plugin_key).is_some())
}

fn ensure_table_for_write(item: &mut TomlItem) -> std::io::Result<&mut dyn toml_edit::TableLike> {
    if item.is_none() {
        *item = TomlItem::Table(new_implicit_table());
    }
    item.as_table_like_mut().ok_or_else(invalid_plugin_table)
}

fn invalid_plugin_table() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "plugins and plugin entries must be tables or inline tables",
    )
}

fn new_implicit_table() -> TomlTable {
    let mut table = TomlTable::new();
    table.set_implicit(true);
    table
}

fn preserve_decor(existing: &TomlItem, replacement: &mut TomlItem) {
    if let (TomlItem::Value(existing_value), TomlItem::Value(replacement_value)) =
        (existing, replacement)
    {
        replacement_value
            .decor_mut()
            .clone_from(existing_value.decor());
    }
}
