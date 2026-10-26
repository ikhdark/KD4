use crate::installed_marketplaces::marketplace_install_root;
use codex_config::RemoveMarketplaceConfigOutcome;
use codex_config::remove_user_marketplace_config;
use codex_plugin::validate_plugin_segment;
use codex_utils_absolute_path::AbsolutePathBuf;
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
    request: MarketplaceRemoveRequest,
) -> Result<MarketplaceRemoveOutcome, MarketplaceRemoveError> {
    tokio::task::spawn_blocking(move || remove_marketplace_sync(codex_home.as_path(), request))
        .await
        .map_err(|err| {
            MarketplaceRemoveError::Internal(format!("failed to remove marketplace: {err}"))
        })?
}

fn remove_marketplace_sync(
    codex_home: &Path,
    request: MarketplaceRemoveRequest,
) -> Result<MarketplaceRemoveOutcome, MarketplaceRemoveError> {
    let marketplace_name = request.marketplace_name;
    validate_plugin_segment(&marketplace_name, "marketplace name")
        .map_err(MarketplaceRemoveError::InvalidRequest)?;
    let _operation = crate::installed_marketplaces::try_lock_marketplace_operation(codex_home, &marketplace_name)
        .map_err(|err| MarketplaceRemoveError::Internal(format!("marketplace operation is busy or unavailable: {err}")))?;

    let destination = marketplace_install_root(codex_home).join(&marketplace_name);
    let staged_root = stage_marketplace_root(&destination)?;
    let removed_installed_root = staged_root.removed_root().cloned();
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

    staged_root.commit();

    Ok(MarketplaceRemoveOutcome {
        marketplace_name,
        removed_installed_root,
    })
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
