use std::path::Path;

use anyhow::Result;
use serde_json::json;
use tempfile::TempDir;

/// Enables plugins and points the ChatGPT-backed plugin services at `base_url`.
pub(crate) fn write_plugins_enabled_config_with_base_url(
    codex_home: &Path,
    base_url: &str,
) -> std::io::Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"chatgpt_base_url = "{base_url}"

[features]
plugins = true
"#
        ),
    )
}

/// Writes a plugin source directory whose manifest declares `app_ids`.
pub(crate) fn write_plugin_source(
    repo_root: &Path,
    plugin_name: &str,
    app_ids: &[&str],
) -> Result<()> {
    let plugin_root = repo_root.join(plugin_name);
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        format!(r#"{{"name":"{plugin_name}"}}"#),
    )?;

    let apps = app_ids
        .iter()
        .map(|app_id| ((*app_id).to_string(), json!({ "id": app_id })))
        .collect::<serde_json::Map<_, _>>();
    std::fs::write(
        plugin_root.join(".app.json"),
        serde_json::to_vec_pretty(&json!({ "apps": apps }))?,
    )?;
    Ok(())
}

/// Writes the cached manifest of a plugin installed from `marketplace_name`.
pub(crate) fn write_installed_plugin(
    codex_home: &TempDir,
    marketplace_name: &str,
    plugin_name: &str,
) -> Result<()> {
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join(marketplace_name)
        .join(plugin_name)
        .join("local/.codex-plugin");
    std::fs::create_dir_all(&plugin_root)?;
    std::fs::write(
        plugin_root.join("plugin.json"),
        format!(r#"{{"name":"{plugin_name}"}}"#),
    )?;
    Ok(())
}
