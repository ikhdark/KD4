use codex_file_system::ExecutorFileSystem;
use codex_utils_path_uri::PathUri;
use std::path::Path;
use std::path::PathBuf;

pub const DISCOVERABLE_PLUGIN_MANIFEST_PATHS: &[&str] =
    &[".codex-plugin/plugin.json", ".claude-plugin/plugin.json"];

pub fn find_plugin_manifest_path(plugin_root: &Path) -> Option<PathBuf> {
    DISCOVERABLE_PLUGIN_MANIFEST_PATHS
        .iter()
        .map(|relative_path| plugin_root.join(relative_path))
        .find(|manifest_path| manifest_path.is_file())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPluginManifestName {
    #[serde(default)]
    name: String,
}

pub async fn plugin_namespace_for_root_uri(
    fs: &dyn ExecutorFileSystem,
    plugin_root: &PathUri,
) -> Option<String> {
    let mut manifest_path = None;
    for relative_path in DISCOVERABLE_PLUGIN_MANIFEST_PATHS {
        let candidate = plugin_root.join(relative_path).ok()?;
        if matches!(fs.get_metadata(&candidate, None).await, Ok(metadata) if metadata.is_file) {
            manifest_path = Some(candidate);
            break;
        }
    }
    let contents = fs.read_file_text(&manifest_path?, None).await.ok()?;
    let RawPluginManifestName { name } = serde_json::from_str(&contents).ok()?;
    Some(
        plugin_root
            .basename()
            .filter(|_| name.trim().is_empty())
            .unwrap_or(name),
    )
}
