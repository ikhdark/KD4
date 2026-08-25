use std::fs;
use std::path::Path;

use crate::OPENAI_API_CURATED_MARKETPLACE_NAME;
use crate::OPENAI_CURATED_MARKETPLACE_NAME;
use crate::PluginsConfigInput;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_config::loader::load_config_layers_state;
use codex_exec_server::LOCAL_FS;
use codex_utils_absolute_path::AbsolutePathBuf;
use toml::Value;

pub(crate) const TEST_CURATED_PLUGIN_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
pub(crate) const TEST_CURATED_PLUGIN_CACHE_VERSION: &str = "01234567";

#[derive(Clone, Copy)]
enum CuratedPluginFixture {
    Complete,
    ManifestOnly,
}

pub(crate) fn write_file(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("file should have a parent")).unwrap();
    fs::write(path, contents).unwrap();
}

pub(crate) fn write_curated_plugin(root: &Path, plugin_name: &str) {
    let plugin_root = root.join("plugins").join(plugin_name);
    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        &format!(
            r#"{{
  "name": "{plugin_name}",
  "description": "Plugin that includes skills, MCP servers, and app connectors"
}}"#
        ),
    );
    write_file(
        &plugin_root.join("skills/SKILL.md"),
        "---\nname: sample\ndescription: sample\n---\n",
    );
    write_file(
        &plugin_root.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "sample-docs": {
      "type": "http",
      "url": "https://sample.example/mcp"
    }
  }
}"#,
    );
    write_file(
        &plugin_root.join(".app.json"),
        r#"{
  "apps": {
    "calendar": {
      "id": "connector_calendar"
    }
  }
}"#,
    );
}

fn write_manifest_only_curated_plugin(root: &Path, plugin_name: &str) {
    let plugin_root = root.join("plugins").join(plugin_name);
    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        &format!(r#"{{"name":"{plugin_name}"}}"#),
    );
}

pub(crate) fn write_openai_curated_marketplace(root: &Path, plugin_names: &[&str]) {
    write_curated_marketplace(
        root,
        "marketplace.json",
        OPENAI_CURATED_MARKETPLACE_NAME,
        /*display_name*/ None,
        plugin_names,
        CuratedPluginFixture::Complete,
    );
}

pub(crate) fn write_manifest_only_openai_curated_marketplace(root: &Path, plugin_names: &[&str]) {
    write_curated_marketplace(
        root,
        "marketplace.json",
        OPENAI_CURATED_MARKETPLACE_NAME,
        /*display_name*/ None,
        plugin_names,
        CuratedPluginFixture::ManifestOnly,
    );
}

pub(crate) fn write_openai_api_curated_marketplace(root: &Path, plugin_names: &[&str]) {
    write_curated_marketplace(
        root,
        "api_marketplace.json",
        OPENAI_API_CURATED_MARKETPLACE_NAME,
        Some("OpenAI Curated"),
        plugin_names,
        CuratedPluginFixture::Complete,
    );
}

fn write_curated_marketplace(
    root: &Path,
    manifest_name: &str,
    marketplace_name: &str,
    display_name: Option<&str>,
    plugin_names: &[&str],
    plugin_fixture: CuratedPluginFixture,
) {
    let plugins = plugin_names
        .iter()
        .map(|plugin_name| {
            format!(
                r#"{{
      "name": "{plugin_name}",
      "source": {{
        "source": "local",
        "path": "./plugins/{plugin_name}"
      }}
    }}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let interface = display_name
        .map(|display_name| {
            format!(
                r#"
  "interface": {{
    "displayName": "{display_name}"
  }},"#
            )
        })
        .unwrap_or_default();
    write_file(
        &root.join(".agents/plugins").join(manifest_name),
        &format!(
            r#"{{
  "name": "{marketplace_name}",{interface}
  "plugins": [
{plugins}
  ]
}}"#
        ),
    );
    for plugin_name in plugin_names {
        match plugin_fixture {
            CuratedPluginFixture::Complete => write_curated_plugin(root, plugin_name),
            CuratedPluginFixture::ManifestOnly => {
                write_manifest_only_curated_plugin(root, plugin_name)
            }
        }
    }
}

pub(crate) fn write_curated_plugin_sha(codex_home: &Path) {
    write_curated_plugin_sha_with(codex_home, TEST_CURATED_PLUGIN_SHA);
}

pub(crate) fn write_curated_plugin_sha_with(codex_home: &Path, sha: &str) {
    write_file(&codex_home.join(".tmp/plugins.sha"), &format!("{sha}\n"));
}

pub(crate) async fn load_plugins_config(codex_home: &Path, cwd: &Path) -> PluginsConfigInput {
    let codex_home = AbsolutePathBuf::try_from(codex_home).expect("codex home should be absolute");
    let cwd = AbsolutePathBuf::try_from(cwd).expect("cwd should be absolute");
    let config_layer_stack = load_config_layers_state(
        LOCAL_FS.as_ref(),
        codex_home.as_path(),
        Some(cwd),
        &[],
        LoaderOverrides::without_managed_config_for_tests(),
        &NoopThreadConfigLoader,
    )
    .await
    .expect("config should load");
    let effective_config = config_layer_stack.effective_config();
    PluginsConfigInput::new(
        config_layer_stack,
        feature_enabled(&effective_config, "plugins", /*default_enabled*/ true),
        feature_enabled(
            &effective_config,
            "remote_plugin",
            /*default_enabled*/ true,
        ),
        "https://chatgpt.com/backend-api/".to_string(),
    )
}

fn feature_enabled(config: &Value, key: &str, default_enabled: bool) -> bool {
    config
        .get("features")
        .and_then(Value::as_table)
        .and_then(|features| features.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(default_enabled)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::write_manifest_only_openai_curated_marketplace;
    use super::write_openai_curated_marketplace;

    #[test]
    fn curated_marketplace_fixture_controls_plugin_payload() {
        let complete = tempdir().unwrap();
        write_openai_curated_marketplace(complete.path(), &["sample"]);
        assert!(
            complete
                .path()
                .join("plugins/sample/skills/SKILL.md")
                .exists()
        );

        let manifest_only = tempdir().unwrap();
        write_manifest_only_openai_curated_marketplace(manifest_only.path(), &["sample"]);
        assert!(
            manifest_only
                .path()
                .join("plugins/sample/.codex-plugin/plugin.json")
                .exists()
        );
        assert!(
            !manifest_only
                .path()
                .join("plugins/sample/skills/SKILL.md")
                .exists()
        );
    }
}
