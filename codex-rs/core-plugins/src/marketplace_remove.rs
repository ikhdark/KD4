use crate::installed_marketplaces::marketplace_install_root;
use crate::installed_marketplaces::resolve_configured_marketplace_root;
use crate::marketplace_upgrade::ConfigLayerReload;
use codex_config::CONFIG_TOML_FILE;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::RemoveMarketplaceConfigOutcome;
use codex_config::format_config_layer_source;
use codex_config::remove_user_marketplace_config;
use codex_plugin::validate_plugin_segment;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::paths_match_after_normalization;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use tempfile::Builder;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceRemoveRequest {
    pub marketplace_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceRemoveOutcome {
    pub marketplace_name: String,
    pub removed_installed_root: Option<AbsolutePathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum MarketplaceRemoveError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("{0}")]
    Internal(String),
}

pub async fn remove_marketplace(
    codex_home: PathBuf,
    reload_config: ConfigLayerReload,
    request: MarketplaceRemoveRequest,
) -> Result<MarketplaceRemoveOutcome, MarketplaceRemoveError> {
    tokio::task::spawn_blocking(move || {
        remove_marketplace_sync(codex_home.as_path(), &reload_config, request)
    })
        .await
        .map_err(|err| {
            MarketplaceRemoveError::Internal(format!("failed to remove marketplace: {err}"))
        })?
}

fn remove_marketplace_sync(
    codex_home: &Path,
    reload_config: &ConfigLayerReload,
    request: MarketplaceRemoveRequest,
) -> Result<MarketplaceRemoveOutcome, MarketplaceRemoveError> {
    let marketplace_name = request.marketplace_name;
    validate_plugin_segment(&marketplace_name, "marketplace name")
        .map_err(MarketplaceRemoveError::InvalidRequest)?;
    let _operation = crate::installed_marketplaces::try_lock_marketplace_operation(codex_home, &marketplace_name)
        .map_err(|err| MarketplaceRemoveError::Internal(format!("marketplace operation is busy or unavailable: {err}")))?;

    let reload_layers = || reload_config().map_err(|err| MarketplaceRemoveError::Internal(
        format!("failed to reload config while removing marketplace: {err}"),
    ));
    ensure_no_other_layer_references(codex_home, &reload_layers()?, &marketplace_name)?;

    let destination = marketplace_install_root(codex_home).join(&marketplace_name);
    let staged_root = stage_marketplace_root(&destination)?;
    let removed_installed_root = staged_root.removed_root().cloned();
    ensure_no_other_layer_references(codex_home, &reload_layers()?, &marketplace_name)?;
    let config_outcome =
        remove_user_marketplace_config(codex_home, &marketplace_name).map_err(|err| {
            MarketplaceRemoveError::Internal(format!(
                "failed to remove marketplace '{marketplace_name}' from user config.toml: {err}"
            ))
        })?;
    if let RemoveMarketplaceConfigOutcome::NameCaseMismatch { configured_name } = &config_outcome {
        return Err(MarketplaceRemoveError::InvalidRequest(format!(
            "marketplace `{marketplace_name}` does not match configured marketplace `{configured_name}` exactly"
        )));
    }

    let removed_config = config_outcome == RemoveMarketplaceConfigOutcome::Removed;

    if removed_installed_root.is_none() && !removed_config {
        return Err(MarketplaceRemoveError::InvalidRequest(format!(
            "marketplace `{marketplace_name}` is not configured or installed"
        )));
    }

    if let Err(err) = ensure_no_other_layer_references(codex_home, &reload_layers()?, &marketplace_name) {
        // A layer added during the base edit now owns this snapshot. The base
        // removal succeeded, but dropping the stage must restore the in-use root.
        if removed_config {
            return Ok(MarketplaceRemoveOutcome { marketplace_name, removed_installed_root: None });
        }
        return Err(err);
    }
    staged_root.commit();

    Ok(MarketplaceRemoveOutcome {
        marketplace_name,
        removed_installed_root,
    })
}

fn ensure_no_other_layer_references(
    codex_home: &Path,
    config_layer_stack: &ConfigLayerStack,
    marketplace_name: &str,
) -> Result<(), MarketplaceRemoveError> {
    // This command edits only the base user config. Do not remove a shared snapshot
    // while a profile, project, system, or session layer still references its name.
    let user_config_file = AbsolutePathBuf::resolve_path_against_base(CONFIG_TOML_FILE, codex_home);
    let install_root = marketplace_install_root(codex_home);
    let destination = install_root.join(marketplace_name);
    if let Some(layer) = config_layer_stack.layers_high_to_low().into_iter().find(|layer| {
        let is_base_user = matches!(
            &layer.name,
            ConfigLayerSource::User { file, profile: None } if file == &user_config_file
        );
        layer.config.get("marketplaces").and_then(toml::Value::as_table)
            .is_some_and(|marketplaces| marketplaces.iter().any(|(name, marketplace)| {
                let same_name = name.eq_ignore_ascii_case(marketplace_name);
                if is_base_user && same_name { return false; }
                same_name || resolve_configured_marketplace_root(name, marketplace, &install_root)
                    .is_some_and(|root| paths_match_after_normalization(root, &destination))
            }))
    }) {
        let source = format_config_layer_source(&layer.name, CONFIG_TOML_FILE);
        return Err(MarketplaceRemoveError::InvalidRequest(format!(
            "marketplace `{marketplace_name}` is configured in {source}; remove it from that configuration source instead"
        )));
    }
    Ok(())
}

struct StagedMarketplaceRoot {
    original_root: PathBuf,
    staged_root: Option<PathBuf>,
    removed_root: Option<AbsolutePathBuf>,
    staging_dir: Option<tempfile::TempDir>,
    committed: bool,
}

impl StagedMarketplaceRoot {
    fn removed_root(&self) -> Option<&AbsolutePathBuf> {
        self.removed_root.as_ref()
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for StagedMarketplaceRoot {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let Some(staged_root) = self.staged_root.as_ref() else {
            return;
        };
        if let Err(err) = fs::rename(staged_root, &self.original_root) {
            let preserved_at = self.staging_dir.take().map(tempfile::TempDir::keep);
            tracing::error!(
                "failed to restore marketplace root {} after config update failure: {err}; staged files preserved at {}",
                self.original_root.display(),
                preserved_at.as_deref().map_or_else(
                    || "<unknown>".to_string(),
                    |path| path.display().to_string()
                )
            );
        }
    }
}

fn stage_marketplace_root(root: &Path) -> Result<StagedMarketplaceRoot, MarketplaceRemoveError> {
    match fs::symlink_metadata(root) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StagedMarketplaceRoot {
                original_root: root.to_path_buf(),
                staged_root: None,
                removed_root: None,
                staging_dir: None,
                committed: false,
            });
        }
        Err(err) => {
            return Err(MarketplaceRemoveError::Internal(format!(
                "failed to inspect installed marketplace root {}: {err}",
                root.display()
            )));
        }
    }

    let removed_root = AbsolutePathBuf::try_from(root.to_path_buf()).map_err(|err| {
        MarketplaceRemoveError::Internal(format!(
            "failed to resolve installed marketplace root {}: {err}",
            root.display()
        ))
    })?;
    let parent = root.parent().ok_or_else(|| {
        MarketplaceRemoveError::Internal(format!(
            "installed marketplace root has no parent: {}",
            root.display()
        ))
    })?;
    let staging_dir = Builder::new()
        .prefix("marketplace-remove-")
        .tempdir_in(parent)
        .map_err(|err| {
            MarketplaceRemoveError::Internal(format!(
                "failed to create marketplace removal staging directory in {}: {err}",
                parent.display()
            ))
        })?;
    let staged_root = staging_dir.path().join("removed-marketplace-root");
    fs::rename(root, &staged_root).map_err(|err| {
        MarketplaceRemoveError::Internal(format!(
            "failed to stage installed marketplace root {} for removal: {err}",
            root.display()
        ))
    })?;
    Ok(StagedMarketplaceRoot {
        original_root: root.to_path_buf(),
        staged_root: Some(staged_root),
        removed_root: Some(removed_root),
        staging_dir: Some(staging_dir),
        committed: false,
    })
}
