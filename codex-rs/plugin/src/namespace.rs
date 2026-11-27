use codex_file_system::ExecutorFileSystem;
use codex_utils_path_uri::PathUri;
use std::io;
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

#[derive(Debug, thiserror::Error)]
#[error("failed to read plugin manifest {path}: {source}")]
pub struct PluginManifestReadError {
    path: PathUri,
    #[source]
    source: io::Error,
}

/// Probes and reads the manifest under `sandbox`, the caller's filesystem authority.
/// Like an absent namespace, a failed read is reported as `None`.
pub async fn plugin_namespace_for_root_uri(
    fs: &dyn ExecutorFileSystem,
    plugin_root: &PathUri,
    sandbox: Option<&codex_file_system::FileSystemSandboxContext>,
) -> Option<String> {
    try_plugin_namespace_for_root_uri(fs, plugin_root, sandbox)
        .await
        .ok()
        .flatten()
}

/// Reports a read failure only after metadata selects a manifest file.
/// Absence and malformed contents still mean no namespace at this root.
pub async fn try_plugin_namespace_for_root_uri(
    fs: &dyn ExecutorFileSystem,
    plugin_root: &PathUri,
    sandbox: Option<&codex_file_system::FileSystemSandboxContext>,
) -> Result<Option<String>, PluginManifestReadError> {
    for relative_path in DISCOVERABLE_PLUGIN_MANIFEST_PATHS {
        let Ok(candidate) = plugin_root.join(relative_path) else {
            return Ok(None);
        };
        if !matches!(fs.get_metadata(&candidate, sandbox).await, Ok(metadata) if metadata.is_file) {
            continue;
        }
        let contents = fs
            .read_file(&candidate, sandbox)
            .await
            .map_err(|source| PluginManifestReadError { path: candidate, source })?;
        // Validate UTF-8 separately so malformed data is not mistaken for a read error.
        let Ok(contents) = std::str::from_utf8(&contents) else {
            return Ok(None);
        };
        let Ok(RawPluginManifestName { name }) = serde_json::from_str(contents) else {
            return Ok(None);
        };
        return Ok(Some(
            plugin_root
                .basename()
                .filter(|_| name.trim().is_empty())
                .unwrap_or(name),
        ));
    }
    Ok(None)
}
