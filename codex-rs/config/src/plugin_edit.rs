use std::path::Path;

use codex_file_system::acquire_atomic_write_lock;
use codex_file_system::resolve_symlink_write_paths;
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
    let _lock = acquire_atomic_write_lock(&config_path)?;
    let write_paths = resolve_symlink_write_paths(&config_path)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn set_user_plugin_enabled_writes_plugin_entry() {
        let codex_home = TempDir::new().unwrap();

        set_user_plugin_enabled(
            codex_home.path(),
            "demo@market".to_string(),
            /*enabled*/ true,
        )
        .await
        .unwrap();

        let config = read_config(codex_home.path());
        let expected: toml::Value = toml::from_str(
            r#"
[plugins."demo@market"]
enabled = true
        "#,
        )
        .unwrap();
        assert_eq!(config, expected);
    }

    #[tokio::test]
    async fn set_user_plugin_enabled_preserves_existing_plugin_fields() {
        let codex_home = TempDir::new().unwrap();
        fs::write(
            codex_home.path().join(CONFIG_TOML_FILE),
            r#"
[plugins."demo@market"]
enabled = false
source = "/tmp/plugin"
"#,
        )
        .unwrap();

        set_user_plugin_enabled(
            codex_home.path(),
            "demo@market".to_string(),
            /*enabled*/ true,
        )
        .await
        .unwrap();

        let config = read_config(codex_home.path());
        let expected: toml::Value = toml::from_str(
            r#"
[plugins."demo@market"]
enabled = true
source = "/tmp/plugin"
        "#,
        )
        .unwrap();
        assert_eq!(config, expected);
    }

    #[tokio::test]
    async fn clear_user_plugin_removes_empty_plugins_table() {
        let codex_home = TempDir::new().unwrap();
        fs::write(
            codex_home.path().join(CONFIG_TOML_FILE),
            r#"
[plugins."demo@market"]
enabled = true
"#,
        )
        .unwrap();

        clear_user_plugin(codex_home.path(), "demo@market".to_string())
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE)).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn clear_user_plugin_missing_entry_does_not_create_config() {
        let codex_home = TempDir::new().unwrap();

        clear_user_plugin(codex_home.path(), "demo@market".to_string())
            .await
            .unwrap();

        assert!(!codex_home.path().join(CONFIG_TOML_FILE).exists());
    }

    #[tokio::test]
    async fn unchanged_plugin_edits_do_not_write_config() -> std::io::Result<()> {
        for original in [
            "plugins = { demo = { enabled = true } } # keep\n",
            "[plugins.demo]\nenabled = false # keep\n",
        ] {
            let home = TempDir::new()?;
            let path = home.path().join(CONFIG_TOML_FILE);
            fs::write(&path, original)?;
            let file = fs::OpenOptions::new().write(true).open(&path)?;
            file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))?;
            drop(file);
            let modified = fs::metadata(&path)?.modified()?;
            let enabled = read_config(home.path())["plugins"]["demo"]["enabled"]
                .as_bool()
                .unwrap();

            apply_user_plugin_config_edits(
                home.path(),
                vec![
                    PluginConfigEdit::SetEnabled {
                        plugin_key: "demo".into(),
                        enabled,
                    },
                    PluginConfigEdit::Clear {
                        plugin_key: "missing".into(),
                    },
                ],
            )
            .await?;

            assert_eq!(fs::read_to_string(&path)?, original);
            assert_eq!(fs::metadata(&path)?.modified()?, modified);
        }
        Ok(())
    }

    fn read_config(codex_home: &Path) -> toml::Value {
        toml::from_str(&fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn repeated_enabled_edits_preserve_file_contents_and_timestamp() -> anyhow::Result<()> {
        for original in [
            "[plugins.demo]\nenabled = true # keep\n",
            "plugins = { demo = { enabled = true, source = 'local' } } # keep\n",
        ] {
            let home = TempDir::new()?;
            let path = home.path().join(CONFIG_TOML_FILE);
            fs::write(&path, original)?;
            fs::File::options()
                .write(true)
                .open(&path)?
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(86400))?;
            let timestamp = fs::metadata(&path)?.modified()?;
            set_user_plugin_enabled(home.path(), "demo".into(), true).await?;
            assert_eq!(fs::read_to_string(&path)?, original);
            assert_eq!(fs::metadata(&path)?.modified()?, timestamp);
        }
        Ok(())
    }

    #[tokio::test]
    async fn plugin_edits_reject_malformed_tables_without_persisting_batch() {
        for contents in [
            "plugins = 1\n",
            "plugins = []\n",
            "[[plugins]]\nenabled = false\n",
            "[plugins]\n'demo@market' = false\n",
            "[plugins]\n'demo@market' = []\n",
            "[[plugins.'demo@market']]\nenabled = false\n",
        ] {
            for edit in [
                PluginConfigEdit::SetEnabled {
                    plugin_key: "demo@market".to_string(),
                    enabled: true,
                },
                PluginConfigEdit::Clear {
                    plugin_key: "demo@market".to_string(),
                },
            ] {
                let temp = TempDir::new().unwrap();
                let path = temp.path().join(CONFIG_TOML_FILE);
                fs::write(&path, contents).unwrap();
                let error = apply_user_plugin_config_edits(
                    temp.path(),
                    vec![
                        PluginConfigEdit::SetEnabled {
                            plugin_key: "valid@market".to_string(),
                            enabled: true,
                        },
                        edit,
                    ],
                )
                .await
                .expect_err("malformed plugin shape must reject the entire batch");
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
                assert_eq!(fs::read_to_string(path).unwrap(), contents);
            }
        }
    }

    #[tokio::test]
    async fn plugin_edits_preserve_inline_tables_and_comments() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(CONFIG_TOML_FILE);
        let contents =
            "plugins = { 'demo@market' = { enabled = false, source = 'local' } } # keep\n";
        fs::write(&path, contents).unwrap();
        set_user_plugin_enabled(temp.path(), "demo@market".to_string(), true)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            contents.replace("false", "true")
        );
        assert_eq!(
            read_config(temp.path())["plugins"]["demo@market"]["enabled"].as_bool(),
            Some(true)
        );
        set_user_plugin_enabled(temp.path(), "added@market".to_string(), false)
            .await
            .unwrap();
        assert_eq!(
            read_config(temp.path())["plugins"]["added@market"]["enabled"].as_bool(),
            Some(false)
        );
        clear_user_plugin(temp.path(), "demo@market".to_string())
            .await
            .unwrap();
        assert!(
            read_config(temp.path())["plugins"]
                .get("demo@market")
                .is_none()
        );
    }

    #[tokio::test]
    async fn inline_plugin_edits_preserve_fields_and_comments() {
        for original in [
            "plugins = { demo = { enabled = false, source = 'local' } } # keep\n",
            "[plugins]\ndemo = { enabled = false, source = 'local' } # keep\n",
            "[plugins.demo]\nenabled = false # keep\nsource = 'local'\n",
        ] {
            let home = TempDir::new().unwrap();
            let path = home.path().join(CONFIG_TOML_FILE);
            fs::write(&path, original).unwrap();
            set_user_plugin_enabled(home.path(), "demo".into(), true)
                .await
                .unwrap();
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                original.replace("false", "true")
            );
            assert_eq!(
                read_config(home.path())["plugins"]["demo"]["enabled"].as_bool(),
                Some(true)
            );
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
                .unwrap();
            let modified = fs::metadata(&path).unwrap().modified().unwrap();
            set_user_plugin_enabled(home.path(), "demo".into(), true)
                .await
                .unwrap();
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                original.replace("false", "true")
            );
        }
    }
}
