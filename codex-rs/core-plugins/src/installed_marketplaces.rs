use std::path::Path;
use std::path::PathBuf;

use codex_config::ConfigLayerStack;
use codex_plugin::validate_plugin_segment;
use codex_utils_absolute_path::AbsolutePathBuf;
use tracing::warn;

use crate::marketplace::find_marketplace_manifest_path;
use crate::marketplace_policy::project_effective_user_config;

pub const INSTALLED_MARKETPLACES_DIR: &str = ".tmp/marketplaces";

pub fn marketplace_install_root(codex_home: &Path) -> PathBuf {
    codex_home.join(INSTALLED_MARKETPLACES_DIR)
}

/// Stable same-marketplace operation lease. Acquire before config locking and
/// retain through filesystem publication, config update, and any rollback.
/// Reuse the existing nonblocking entry-lock owner; unrelated names do not wait.
pub(crate) fn try_lock_marketplace_operation(codex_home: &Path, name: &str) -> std::io::Result<std::fs::File> {
    validate_plugin_segment(name, "marketplace name").map_err(std::io::Error::other)?;
    crate::store::try_lock_plugin_cache_entry(&marketplace_install_root(codex_home).join(name))
}

pub fn installed_marketplace_roots_from_layer_stack(
    config_layer_stack: &ConfigLayerStack,
    codex_home: &Path,
) -> Vec<AbsolutePathBuf> {
    let user_config = project_effective_user_config(config_layer_stack, codex_home);
    let Some(marketplaces_value) = user_config.get("marketplaces") else {
        return Vec::new();
    };
    let Some(marketplaces) = marketplaces_value.as_table() else {
        warn!("invalid marketplaces config: expected table");
        return Vec::new();
    };
    let default_install_root = marketplace_install_root(codex_home);
    let mut roots = marketplaces
        .iter()
        .filter_map(|(marketplace_name, marketplace)| {
            if !marketplace.is_table() {
                warn!(
                    marketplace_name,
                    "ignoring invalid configured marketplace entry"
                );
                return None;
            }
            if let Err(err) = validate_plugin_segment(marketplace_name, "marketplace name") {
                warn!(
                    marketplace_name,
                    error = %err,
                    "ignoring invalid configured marketplace name"
                );
                return None;
            }
            let path = resolve_configured_marketplace_root(
                marketplace_name,
                marketplace,
                &default_install_root,
            )?;
            find_marketplace_manifest_path(&path).map(|_| path)
        })
        .filter_map(|path| AbsolutePathBuf::try_from(path).ok())
        .collect::<Vec<_>>();
    roots.sort_unstable_by(|left, right| left.as_path().cmp(right.as_path()));
    roots
}

/// Source binding is independent of revision: a compatible source may advance.
pub(crate) fn configured_marketplace_source_identity(
    marketplace_name: &str,
    marketplace: &toml::Value,
) -> Result<String, String> {
    match marketplace.get("source_type").and_then(toml::Value::as_str) {
        Some("git") => crate::marketplace_upgrade::configured_git_marketplace_source_identity(marketplace_name, marketplace),
        Some("local") => {
            let source = marketplace.get("source").and_then(toml::Value::as_str)
                .filter(|source| !source.is_empty()).ok_or_else(|| "local marketplace is missing source".to_string())?;
            local_marketplace_source_identity(Path::new(source))
        }
        _ => Err(format!("marketplace `{marketplace_name}` has no verifiable source; add or upgrade it again")),
    }
}

fn local_marketplace_source_identity(root: &Path) -> Result<String, String> {
    let root = std::fs::canonicalize(root)
        .map_err(|err| format!("failed to resolve marketplace source: {err}"))?;
    serde_json::to_string(&("local", root)).map_err(|err| err.to_string())
}

pub(crate) fn marketplace_source_identity(root: &Path) -> Result<String, String> {
    crate::marketplace_upgrade::installed_marketplace_source_identity(root)?
        .map_or_else(|| local_marketplace_source_identity(root), Ok)
}

pub fn resolve_configured_marketplace_root(
    marketplace_name: &str,
    marketplace: &toml::Value,
    default_install_root: &Path,
) -> Option<PathBuf> {
    match marketplace.get("source_type").and_then(toml::Value::as_str) {
        Some("local") => marketplace.get("source").and_then(toml::Value::as_str)
            .filter(|source| !source.is_empty()).map(PathBuf::from),
        Some("git") => {
            let root = default_install_root.join(marketplace_name);
            let expected = configured_marketplace_source_identity(marketplace_name, marketplace).ok()?;
            (crate::marketplace_upgrade::installed_marketplace_source_identity(&root).ok().flatten().as_ref()
                == Some(&expected)).then_some(root)
        }
        // Name-only legacy installs have no evidence of their configured source.
        // Re-adding/upgrading re-materializes them and writes the owning metadata.
        _ => None,
    }
}
