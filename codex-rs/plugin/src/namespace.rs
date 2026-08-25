use codex_exec_server::ExecutorFileSystem;
use codex_utils_absolute_path::AbsolutePathBuf;
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

pub async fn plugin_namespace_for_skill_path(
    fs: &dyn ExecutorFileSystem,
    path: &AbsolutePathBuf,
) -> Option<String> {
    plugin_namespace_for_skill_uri(fs, &PathUri::from_abs_path(path)).await
}

pub async fn plugin_namespace_for_skill_uri(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
) -> Option<String> {
    let mut ancestor = Some(path.clone());
    while let Some(path) = ancestor {
        if let Some(name) = plugin_namespace_for_root_uri(fs, &path).await {
            return Some(name);
        }
        ancestor = path.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_exec_server::LOCAL_FS;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use std::fs;

    #[tokio::test]
    async fn resolves_nearest_manifest_name() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sample");
        let skill = root.join("skills/search/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::create_dir_all(root.join(".codex-plugin")).unwrap();
        fs::write(
            root.join(".codex-plugin/plugin.json"),
            r#"{"name":"sample"}"#,
        )
        .unwrap();
        assert_eq!(
            plugin_namespace_for_skill_path(LOCAL_FS.as_ref(), &skill.abs()).await,
            Some("sample".to_string())
        );
    }

    #[tokio::test]
    async fn recognizes_alternate_manifest_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sample");
        let skill = root.join("skills/search/SKILL.md");
        let manifest = root.join(".claude-plugin/plugin.json");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(&manifest, r#"{"name":"alternate"}"#).unwrap();

        assert_eq!(find_plugin_manifest_path(&root), Some(manifest));
        assert_eq!(
            plugin_namespace_for_skill_path(LOCAL_FS.as_ref(), &skill.abs()).await,
            Some("alternate".to_string())
        );
    }
}
