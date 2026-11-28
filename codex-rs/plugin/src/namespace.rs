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
#[error("failed to {operation} plugin manifest {path}: {source}")]
pub struct PluginManifestReadError {
    operation: &'static str,
    path: PathUri,
    #[source]
    source: io::Error,
}

/// Probes and reads the manifest under `sandbox`, the caller's filesystem authority.
/// Skips failed metadata probes and reports a failed read as `None`.
pub async fn plugin_namespace_for_root_uri(
    fs: &dyn ExecutorFileSystem,
    plugin_root: &PathUri,
    sandbox: Option<&codex_file_system::FileSystemSandboxContext>,
) -> Option<String> {
    resolve_plugin_namespace_for_root_uri(fs, plugin_root, sandbox, /*report_metadata_errors*/ false)
        .await
        .ok()
        .flatten()
}

/// Reports manifest probe failures other than missing candidates, and selected-file read failures.
/// Absence and malformed contents still mean no namespace at this root.
pub async fn try_plugin_namespace_for_root_uri(
    fs: &dyn ExecutorFileSystem,
    plugin_root: &PathUri,
    sandbox: Option<&codex_file_system::FileSystemSandboxContext>,
) -> Result<Option<String>, PluginManifestReadError> {
    resolve_plugin_namespace_for_root_uri(fs, plugin_root, sandbox, /*report_metadata_errors*/ true)
        .await
}

async fn resolve_plugin_namespace_for_root_uri(
    fs: &dyn ExecutorFileSystem,
    plugin_root: &PathUri,
    sandbox: Option<&codex_file_system::FileSystemSandboxContext>,
    report_metadata_errors: bool,
) -> Result<Option<String>, PluginManifestReadError> {
    for relative_path in DISCOVERABLE_PLUGIN_MANIFEST_PATHS {
        let Ok(candidate) = plugin_root.join(relative_path) else {
            return Ok(None);
        };
        match fs.get_metadata(&candidate, sandbox).await {
            Ok(metadata) if metadata.is_file => {}
            Err(source) if report_metadata_errors && source.kind() != io::ErrorKind::NotFound => {
                return Err(PluginManifestReadError {
                    operation: "get metadata for",
                    path: candidate,
                    source,
                });
            }
            Ok(_) | Err(_) => continue,
        }
        let contents = fs
            .read_file(&candidate, sandbox)
            .await
            .map_err(|source| PluginManifestReadError {
                operation: "read",
                path: candidate,
                source,
            })?;
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
