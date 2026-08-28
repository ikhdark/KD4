use crate::manifest::PluginManifest;
use crate::manifest::load_plugin_manifest;
use crate::manifest::parse_plugin_manifest;
use codex_file_system::write_bytes_atomically;
use codex_plugin::PluginId;
use codex_plugin::find_plugin_manifest_path;
use codex_plugin::validate_plugin_segment;
use codex_utils_absolute_path::AbsolutePathBuf;
use semver::Version;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use tracing::warn;

pub const DEFAULT_PLUGIN_VERSION: &str = "local";
pub const PLUGINS_CACHE_DIR: &str = "plugins/cache";
pub const PLUGINS_DATA_DIR: &str = "plugins/data";
const REMOTE_PLUGIN_INSTALL_METADATA_FILE: &str = ".codex-remote-plugin-install.json";
const REMOTE_PLUGIN_INSTALL_METADATA_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Deserialize, Serialize)]
struct RemotePluginInstallMetadata {
    schema_version: u8,
    remote_plugin_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallResult {
    pub plugin_id: PluginId,
    pub plugin_version: String,
    pub installed_path: AbsolutePathBuf,
}

#[derive(Debug)]
pub(crate) struct PendingPluginInstall {
    result: PluginInstallResult,
    target_root: PathBuf,
    staged_root: PathBuf,
    backup_root: Option<PathBuf>,
    transaction_dir: Option<tempfile::TempDir>,
    committed: bool,
}

impl PendingPluginInstall {
    pub(crate) fn result(&self) -> &PluginInstallResult {
        &self.result
    }

    pub(crate) fn commit(mut self) -> PluginInstallResult {
        self.committed = true;
        self.result.clone()
    }

    fn rollback(&mut self) -> io::Result<()> {
        if self.target_root.exists() {
            fs::rename(&self.target_root, &self.staged_root)?;
        }
        if let Some(backup_root) = self.backup_root.as_ref()
            && backup_root.exists()
            && let Err(err) = fs::rename(backup_root, &self.target_root)
        {
            if self.staged_root.exists() {
                let _ = fs::rename(&self.staged_root, &self.target_root);
            }
            return Err(err);
        }
        Ok(())
    }
}

impl Drop for PendingPluginInstall {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Err(err) = self.rollback() {
            let preserved_at = self.transaction_dir.take().map(tempfile::TempDir::keep);
            warn!(
                "failed to roll back plugin install at {}: {err}; transaction files preserved at {}",
                self.target_root.display(),
                preserved_at.as_deref().map_or_else(
                    || "<unknown>".to_string(),
                    |path| path.display().to_string()
                )
            );
        }
    }
}

#[derive(Debug)]
pub(crate) struct PendingPluginUninstall {
    target_root: PathBuf,
    backup_root: Option<PathBuf>,
    transaction_dir: Option<tempfile::TempDir>,
    committed: bool,
}

impl PendingPluginUninstall {
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for PendingPluginUninstall {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let Some(backup_root) = self.backup_root.as_ref() else {
            return;
        };
        if !backup_root.exists() {
            return;
        }
        if let Err(err) = fs::rename(backup_root, &self.target_root) {
            let preserved_at = self.transaction_dir.take().map(tempfile::TempDir::keep);
            warn!(
                "failed to roll back plugin uninstall at {}: {err}; plugin files preserved at {}",
                self.target_root.display(),
                preserved_at.as_deref().map_or_else(
                    || "<unknown>".to_string(),
                    |path| path.display().to_string()
                )
            );
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginStore {
    codex_home: AbsolutePathBuf,
    root: AbsolutePathBuf,
    data_root: AbsolutePathBuf,
}

#[derive(Clone, Copy)]
enum InstallManifest<'a> {
    OnDisk,
    Fallback(&'a str),
}

impl PluginStore {
    pub fn new(codex_home: PathBuf) -> Self {
        Self::try_new(codex_home)
            .unwrap_or_else(|err| panic!("plugin cache root should be absolute: {err}"))
    }

    pub fn try_new(codex_home: PathBuf) -> Result<Self, PluginStoreError> {
        let root = AbsolutePathBuf::from_absolute_path_checked(codex_home.join(PLUGINS_CACHE_DIR))
            .map_err(|err| PluginStoreError::io("failed to resolve plugin cache root", err))?;
        let data_root =
            AbsolutePathBuf::from_absolute_path_checked(codex_home.join(PLUGINS_DATA_DIR))
                .map_err(|err| PluginStoreError::io("failed to resolve plugin data root", err))?;
        let codex_home = AbsolutePathBuf::from_absolute_path_checked(codex_home)
            .map_err(|err| PluginStoreError::io("failed to resolve Codex home", err))?;

        Ok(Self {
            codex_home,
            root,
            data_root,
        })
    }

    pub fn root(&self) -> &AbsolutePathBuf {
        &self.root
    }

    pub(crate) fn codex_home(&self) -> &AbsolutePathBuf {
        &self.codex_home
    }

    pub fn plugin_base_root(&self, plugin_id: &PluginId) -> AbsolutePathBuf {
        self.root
            .join(plugin_id.marketplace_name())
            .join(plugin_id.plugin_name())
    }

    pub fn plugin_root(&self, plugin_id: &PluginId, plugin_version: &str) -> AbsolutePathBuf {
        self.plugin_base_root(plugin_id).join(plugin_version)
    }

    pub fn plugin_data_root(&self, plugin_id: &PluginId) -> AbsolutePathBuf {
        self.data_root
            .join(plugin_id.marketplace_name())
            .join(plugin_id.plugin_name())
    }

    fn legacy_plugin_data_root(&self, plugin_id: &PluginId) -> AbsolutePathBuf {
        self.data_root.join(format!(
            "{}-{}",
            plugin_id.plugin_name(),
            plugin_id.marketplace_name()
        ))
    }

    pub(crate) fn migrate_legacy_plugin_data_roots(&self, plugin_ids: &[PluginId]) {
        let mut plugin_ids_by_legacy_root = BTreeMap::<PathBuf, Vec<&PluginId>>::new();
        for plugin_id in plugin_ids {
            plugin_ids_by_legacy_root
                .entry(
                    self.legacy_plugin_data_root(plugin_id)
                        .as_path()
                        .to_path_buf(),
                )
                .or_default()
                .push(plugin_id);
        }

        for (legacy_root, plugin_ids) in plugin_ids_by_legacy_root {
            if !legacy_root.exists() {
                continue;
            }
            if plugin_ids.len() != 1 {
                let plugin_keys = plugin_ids
                    .iter()
                    .map(|plugin_id| plugin_id.as_key())
                    .collect::<Vec<_>>();
                warn!(
                    legacy_path = %legacy_root.display(),
                    plugins = ?plugin_keys,
                    "legacy plugin data directory is ambiguous; leaving it unmigrated"
                );
                continue;
            }

            let plugin_id = plugin_ids[0];
            let destination = self.plugin_data_root(plugin_id);
            if destination.as_path().exists() {
                warn!(
                    plugin = plugin_id.as_key(),
                    legacy_path = %legacy_root.display(),
                    destination = %destination.as_path().display(),
                    "legacy and nested plugin data directories both exist; leaving the legacy directory unmigrated"
                );
                continue;
            }
            let Some(destination_parent) = destination.as_path().parent() else {
                warn!(
                    plugin = plugin_id.as_key(),
                    destination = %destination.as_path().display(),
                    "nested plugin data directory has no parent; leaving the legacy directory unmigrated"
                );
                continue;
            };
            if let Err(err) = fs::create_dir_all(destination_parent) {
                warn!(
                    plugin = plugin_id.as_key(),
                    destination = %destination.as_path().display(),
                    "failed to create nested plugin data parent; leaving the legacy directory unmigrated: {err}"
                );
                continue;
            }
            if let Err(err) = fs::rename(&legacy_root, destination.as_path()) {
                warn!(
                    plugin = plugin_id.as_key(),
                    legacy_path = %legacy_root.display(),
                    destination = %destination.as_path().display(),
                    "failed to migrate legacy plugin data directory: {err}"
                );
            }
        }
    }

    pub fn active_plugin_version(&self, plugin_id: &PluginId) -> Option<String> {
        let mut discovered_versions = fs::read_dir(self.plugin_base_root(plugin_id).as_path())
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry.file_type().ok().filter(std::fs::FileType::is_dir)?;
                entry.file_name().into_string().ok()
            })
            .filter(|version| validate_plugin_version_segment(version).is_ok())
            .collect::<Vec<_>>();
        discovered_versions.sort_unstable_by(|left, right| compare_plugin_versions(left, right));
        if discovered_versions.is_empty() {
            None
        } else if discovered_versions
            .iter()
            .any(|version| version == DEFAULT_PLUGIN_VERSION)
        {
            Some(DEFAULT_PLUGIN_VERSION.to_string())
        } else {
            discovered_versions.pop()
        }
    }

    pub fn active_plugin_root(&self, plugin_id: &PluginId) -> Option<AbsolutePathBuf> {
        self.active_plugin_version(plugin_id)
            .map(|plugin_version| self.plugin_root(plugin_id, &plugin_version))
    }

    pub fn is_installed(&self, plugin_id: &PluginId) -> bool {
        self.active_plugin_version(plugin_id).is_some()
    }

    pub fn remote_plugin_id(
        &self,
        plugin_id: &PluginId,
    ) -> Result<Option<String>, PluginStoreError> {
        if !self.is_installed(plugin_id) {
            return Ok(None);
        }
        let path = self.remote_plugin_install_metadata_path(plugin_id);
        let contents = match fs::read_to_string(path.as_path()) {
            Ok(contents) => contents,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(PluginStoreError::io(
                    "failed to read remote plugin install metadata",
                    err,
                ));
            }
        };
        let metadata: RemotePluginInstallMetadata =
            serde_json::from_str(&contents).map_err(|err| {
                PluginStoreError::Invalid(format!(
                    "failed to parse remote plugin install metadata: {err}"
                ))
            })?;
        if metadata.schema_version != REMOTE_PLUGIN_INSTALL_METADATA_SCHEMA_VERSION {
            return Err(PluginStoreError::Invalid(format!(
                "unsupported remote plugin install metadata schema version: {}",
                metadata.schema_version
            )));
        }
        let remote_plugin_id = metadata.remote_plugin_id.trim();
        if remote_plugin_id.is_empty() {
            return Err(PluginStoreError::Invalid(
                "invalid remote plugin install metadata: remote plugin id must not be blank"
                    .to_string(),
            ));
        }
        Ok(Some(remote_plugin_id.to_string()))
    }

    pub fn write_remote_plugin_id(
        &self,
        plugin_id: &PluginId,
        remote_plugin_id: &str,
    ) -> Result<(), PluginStoreError> {
        if !self.is_installed(plugin_id) {
            return Err(PluginStoreError::Invalid(format!(
                "cannot write remote identity for uninstalled plugin `{}`",
                plugin_id.as_key()
            )));
        }
        let remote_plugin_id = remote_plugin_id.trim();
        if remote_plugin_id.is_empty() {
            return Err(PluginStoreError::Invalid(
                "invalid remote plugin install metadata: remote plugin id must not be blank"
                    .to_string(),
            ));
        }
        let path = self.remote_plugin_install_metadata_path(plugin_id);
        let mut contents = serde_json::to_vec_pretty(&RemotePluginInstallMetadata {
            schema_version: REMOTE_PLUGIN_INSTALL_METADATA_SCHEMA_VERSION,
            remote_plugin_id: remote_plugin_id.to_string(),
        })
        .map_err(|err| {
            PluginStoreError::Invalid(format!(
                "failed to serialize remote plugin install metadata: {err}"
            ))
        })?;
        contents.push(b'\n');
        write_bytes_atomically(path.as_path(), &contents).map_err(|err| {
            PluginStoreError::io("failed to write remote plugin install metadata", err)
        })?;
        Ok(())
    }

    pub fn install(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        self.begin_install(source_path, plugin_id)
            .map(PendingPluginInstall::commit)
    }

    pub(crate) fn begin_install(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
    ) -> Result<PendingPluginInstall, PluginStoreError> {
        self.begin_install_with_manifest(source_path, plugin_id, InstallManifest::OnDisk)
    }

    #[cfg(test)]
    pub(crate) fn install_with_fallback_manifest(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        manifest_contents: &str,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        self.begin_install_with_fallback_manifest(source_path, plugin_id, manifest_contents)
            .map(PendingPluginInstall::commit)
    }

    pub(crate) fn begin_install_with_fallback_manifest(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        manifest_contents: &str,
    ) -> Result<PendingPluginInstall, PluginStoreError> {
        self.begin_install_with_manifest(
            source_path,
            plugin_id,
            InstallManifest::Fallback(manifest_contents),
        )
    }

    pub fn install_with_version(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        plugin_version: String,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        self.begin_install_with_version(source_path, plugin_id, plugin_version)
            .map(PendingPluginInstall::commit)
    }

    pub(crate) fn begin_install_with_version(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        plugin_version: String,
    ) -> Result<PendingPluginInstall, PluginStoreError> {
        self.begin_install_with_version_and_manifest(
            source_path,
            plugin_id,
            plugin_version,
            InstallManifest::OnDisk,
        )
    }

    pub(crate) fn install_with_version_and_fallback_manifest(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        plugin_version: String,
        manifest_contents: &str,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        self.begin_install_with_version_and_fallback_manifest(
            source_path,
            plugin_id,
            plugin_version,
            manifest_contents,
        )
        .map(PendingPluginInstall::commit)
    }

    pub(crate) fn begin_install_with_version_and_fallback_manifest(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        plugin_version: String,
        manifest_contents: &str,
    ) -> Result<PendingPluginInstall, PluginStoreError> {
        self.begin_install_with_version_and_manifest(
            source_path,
            plugin_id,
            plugin_version,
            InstallManifest::Fallback(manifest_contents),
        )
    }

    fn begin_install_with_manifest(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        manifest: InstallManifest<'_>,
    ) -> Result<PendingPluginInstall, PluginStoreError> {
        let manifest = resolve_install_manifest(source_path.as_path(), manifest);
        let plugin_version = plugin_version_for_install_manifest(source_path.as_path(), manifest)?;
        self.begin_install_with_version_and_manifest(
            source_path,
            plugin_id,
            plugin_version,
            manifest,
        )
    }

    fn begin_install_with_version_and_manifest(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        plugin_version: String,
        manifest: InstallManifest<'_>,
    ) -> Result<PendingPluginInstall, PluginStoreError> {
        if !source_path.as_path().is_dir() {
            return Err(PluginStoreError::Invalid(format!(
                "plugin source path is not a directory: {}",
                source_path.display()
            )));
        }

        let manifest = resolve_install_manifest(source_path.as_path(), manifest);
        let plugin_name = plugin_name_for_source(source_path.as_path(), manifest)?;
        if plugin_name != plugin_id.plugin_name() {
            return Err(PluginStoreError::Invalid(format!(
                "plugin.json name `{plugin_name}` does not match marketplace plugin name `{}`",
                plugin_id.plugin_name()
            )));
        }
        validate_plugin_version_segment(&plugin_version).map_err(PluginStoreError::Invalid)?;
        let installed_path = self.plugin_root(&plugin_id, &plugin_version);
        replace_plugin_root_atomically(
            source_path.as_path(),
            self.plugin_base_root(&plugin_id).as_path(),
            &plugin_version,
            manifest,
            PluginInstallResult {
                plugin_id,
                plugin_version: plugin_version.clone(),
                installed_path,
            },
        )
    }

    pub fn uninstall(&self, plugin_id: &PluginId) -> Result<(), PluginStoreError> {
        self.begin_uninstall(plugin_id)
            .map(PendingPluginUninstall::commit)
    }

    pub(crate) fn begin_uninstall(
        &self,
        plugin_id: &PluginId,
    ) -> Result<PendingPluginUninstall, PluginStoreError> {
        stage_plugin_uninstall(self.plugin_base_root(plugin_id).as_path())
    }

    fn remote_plugin_install_metadata_path(&self, plugin_id: &PluginId) -> AbsolutePathBuf {
        self.plugin_base_root(plugin_id)
            .join(REMOTE_PLUGIN_INSTALL_METADATA_FILE)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PluginStoreError {
    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("{0}")]
    Invalid(String),
}

impl PluginStoreError {
    fn io(context: &'static str, source: io::Error) -> Self {
        Self::Io { context, source }
    }

    pub(crate) fn sub_error_type(&self) -> Option<String> {
        match self {
            Self::Io { context, .. } => Some(error_context_sub_error_type(context)),
            Self::Invalid(_) => None,
        }
    }
}

pub(crate) fn error_context_sub_error_type(context: &str) -> String {
    context.to_ascii_lowercase().replace(' ', "_")
}

pub fn plugin_version_for_source(source_path: &Path) -> Result<String, PluginStoreError> {
    plugin_version_for_install_manifest(source_path, InstallManifest::OnDisk)
}

pub(crate) fn plugin_version_for_source_with_fallback_manifest(
    source_path: &Path,
    manifest_contents: &str,
) -> Result<String, PluginStoreError> {
    let manifest =
        resolve_install_manifest(source_path, InstallManifest::Fallback(manifest_contents));
    plugin_version_for_install_manifest(source_path, manifest)
}

fn resolve_install_manifest<'a>(
    source_path: &Path,
    manifest: InstallManifest<'a>,
) -> InstallManifest<'a> {
    // A real plugin manifest always wins. The fallback only fills the gap for marketplace
    // sources that cannot be changed in place because they may be user-owned directories.
    match manifest {
        InstallManifest::Fallback(_) if find_plugin_manifest_path(source_path).is_some() => {
            InstallManifest::OnDisk
        }
        manifest => manifest,
    }
}

fn plugin_version_for_install_manifest(
    source_path: &Path,
    manifest: InstallManifest<'_>,
) -> Result<String, PluginStoreError> {
    let plugin_version = plugin_manifest_version_for_source(source_path, manifest)?
        .unwrap_or_else(|| DEFAULT_PLUGIN_VERSION.to_string());
    validate_plugin_version_segment(&plugin_version).map_err(PluginStoreError::Invalid)?;
    Ok(plugin_version)
}

pub fn validate_plugin_version_segment(plugin_version: &str) -> Result<(), String> {
    if plugin_version.is_empty() {
        return Err("invalid plugin version: must not be empty".to_string());
    }
    if matches!(plugin_version, "." | "..") {
        return Err("invalid plugin version: path traversal is not allowed".to_string());
    }
    if !plugin_version
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+'))
    {
        return Err(
            "invalid plugin version: only ASCII letters, digits, `.`, `+`, `_`, and `-` are allowed"
                .to_string(),
        );
    }
    Ok(())
}

fn plugin_manifest_for_source(
    source_path: &Path,
    manifest: InstallManifest<'_>,
) -> Result<PluginManifest, PluginStoreError> {
    match manifest {
        InstallManifest::OnDisk => load_plugin_manifest(source_path)
            .ok_or_else(|| PluginStoreError::Invalid("missing or invalid plugin.json".to_string())),
        InstallManifest::Fallback(contents) => parse_plugin_manifest(
            source_path,
            &source_path.join(".codex-plugin/plugin.json"),
            contents,
        )
        .map_err(|err| PluginStoreError::Invalid(format!("failed to parse plugin.json: {err}"))),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPluginManifestVersion {
    #[serde(default)]
    version: Option<JsonValue>,
}

fn plugin_manifest_version_for_source(
    source_path: &Path,
    manifest: InstallManifest<'_>,
) -> Result<Option<String>, PluginStoreError> {
    let contents = match manifest {
        InstallManifest::OnDisk => {
            let manifest_path = find_plugin_manifest_path(source_path)
                .ok_or_else(|| PluginStoreError::Invalid("missing plugin.json".to_string()))?;
            fs::read_to_string(&manifest_path)
                .map_err(|err| PluginStoreError::io("failed to read plugin.json", err))?
        }
        InstallManifest::Fallback(contents) => contents.to_string(),
    };
    let manifest: RawPluginManifestVersion = serde_json::from_str(&contents)
        .map_err(|err| PluginStoreError::Invalid(format!("failed to parse plugin.json: {err}")))?;
    let Some(version) = manifest.version else {
        return Ok(None);
    };
    let Some(version) = version.as_str() else {
        return Err(PluginStoreError::Invalid(
            "invalid plugin version in plugin.json: expected string".to_string(),
        ));
    };
    let version = version.trim();
    if version.is_empty() {
        return Err(PluginStoreError::Invalid(
            "invalid plugin version in plugin.json: must not be blank".to_string(),
        ));
    }
    Ok(Some(version.to_string()))
}

fn plugin_name_for_source(
    source_path: &Path,
    manifest: InstallManifest<'_>,
) -> Result<String, PluginStoreError> {
    let manifest = plugin_manifest_for_source(source_path, manifest)?;

    let plugin_name = manifest.name;
    validate_plugin_segment(&plugin_name, "plugin name")
        .map_err(PluginStoreError::Invalid)
        .map(|_| plugin_name)
}

fn replace_plugin_root_atomically(
    source: &Path,
    target_root: &Path,
    plugin_version: &str,
    manifest: InstallManifest<'_>,
    result: PluginInstallResult,
) -> Result<PendingPluginInstall, PluginStoreError> {
    let Some(parent) = target_root.parent() else {
        return Err(PluginStoreError::Invalid(format!(
            "plugin cache path has no parent: {}",
            target_root.display()
        )));
    };

    fs::create_dir_all(parent)
        .map_err(|err| PluginStoreError::io("failed to create plugin cache directory", err))?;

    let Some(plugin_dir_name) = target_root.file_name() else {
        return Err(PluginStoreError::Invalid(format!(
            "plugin cache path has no directory name: {}",
            target_root.display()
        )));
    };
    let staged_dir = tempfile::Builder::new()
        .prefix("plugin-install-")
        .tempdir_in(parent)
        .map_err(|err| {
            PluginStoreError::io("failed to create temporary plugin cache directory", err)
        })?;
    let staged_root = staged_dir.path().join(plugin_dir_name);
    let staged_version_root = staged_root.join(plugin_version);
    copy_dir_recursive(source, &staged_version_root)?;
    if let InstallManifest::Fallback(contents) = manifest {
        // Inject the generated manifest into Store's existing atomic copy so install does not
        // mutate the original source or require a second staging directory.
        let manifest_path = staged_version_root.join(".codex-plugin/plugin.json");
        let Some(manifest_parent) = manifest_path.parent() else {
            return Err(PluginStoreError::Invalid(
                "plugin manifest path has no parent".to_string(),
            ));
        };
        fs::create_dir_all(manifest_parent).map_err(|err| {
            PluginStoreError::io("failed to create plugin manifest directory", err)
        })?;
        fs::write(&manifest_path, contents)
            .map_err(|err| PluginStoreError::io("failed to write fallback plugin manifest", err))?;
    }

    let backup_root = target_root
        .exists()
        .then(|| staged_dir.path().join("previous-plugin-root"));
    if let Some(backup_root) = backup_root.as_ref() {
        fs::rename(target_root, backup_root)
            .map_err(|err| PluginStoreError::io("failed to back up plugin cache entry", err))?;
    }

    let transaction = PendingPluginInstall {
        result,
        target_root: target_root.to_path_buf(),
        staged_root,
        backup_root,
        transaction_dir: Some(staged_dir),
        committed: false,
    };
    if let Err(err) = fs::rename(&transaction.staged_root, target_root) {
        return Err(PluginStoreError::io(
            "failed to activate updated plugin cache entry",
            err,
        ));
    }
    Ok(transaction)
}

fn stage_plugin_uninstall(path: &Path) -> Result<PendingPluginUninstall, PluginStoreError> {
    if !path.exists() {
        return Ok(PendingPluginUninstall {
            target_root: path.to_path_buf(),
            backup_root: None,
            transaction_dir: None,
            committed: false,
        });
    }
    let parent = path.parent().ok_or_else(|| {
        PluginStoreError::Invalid(format!(
            "plugin cache path has no parent: {}",
            path.display()
        ))
    })?;
    let transaction_dir = tempfile::Builder::new()
        .prefix("plugin-uninstall-")
        .tempdir_in(parent)
        .map_err(|err| {
            PluginStoreError::io("failed to create plugin uninstall staging directory", err)
        })?;
    let backup_root = transaction_dir.path().join("removed-plugin-root");
    fs::rename(path, &backup_root)
        .map_err(|err| PluginStoreError::io("failed to stage plugin cache removal", err))?;
    Ok(PendingPluginUninstall {
        target_root: path.to_path_buf(),
        backup_root: Some(backup_root),
        transaction_dir: Some(transaction_dir),
        committed: false,
    })
}

fn compare_plugin_versions(left: &str, right: &str) -> Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<(), PluginStoreError> {
    fs::create_dir_all(target)
        .map_err(|err| PluginStoreError::io("failed to create plugin target directory", err))?;

    for entry in fs::read_dir(source)
        .map_err(|err| PluginStoreError::io("failed to read plugin source directory", err))?
    {
        let entry =
            entry.map_err(|err| PluginStoreError::io("failed to enumerate plugin source", err))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|err| PluginStoreError::io("failed to inspect plugin source entry", err))?;

        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &target_path)
                .map_err(|err| PluginStoreError::io("failed to copy plugin file", err))?;
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
