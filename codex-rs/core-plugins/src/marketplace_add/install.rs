use super::MarketplaceAddError;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

pub(super) fn clone_git_source(
    url: &str,
    ref_name: Option<&str>,
    sparse_paths: &[String],
    destination: &Path,
) -> Result<String, MarketplaceAddError> {
    crate::marketplace_upgrade::git::clone_git_source(
        url,
        ref_name,
        sparse_paths,
        destination,
        Duration::from_secs(30),
    )
    .map_err(MarketplaceAddError::Internal)
}

pub(super) fn safe_marketplace_dir_name(
    marketplace_name: &str,
) -> Result<String, MarketplaceAddError> {
    let safe = marketplace_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    let safe = safe.trim_matches('.').to_string();
    if safe.is_empty() || safe == ".." {
        return Err(MarketplaceAddError::InvalidRequest(format!(
            "marketplace name '{marketplace_name}' cannot be used as an install directory"
        )));
    }
    Ok(safe)
}

pub(super) fn ensure_marketplace_destination_is_inside_install_root(
    install_root: &Path,
    destination: &Path,
) -> Result<(), MarketplaceAddError> {
    let install_root = install_root.canonicalize().map_err(|err| {
        MarketplaceAddError::Internal(format!(
            "failed to resolve marketplace install root {}: {err}",
            install_root.display()
        ))
    })?;
    let destination_parent = destination
        .parent()
        .ok_or_else(|| {
            MarketplaceAddError::Internal("marketplace destination has no parent".to_string())
        })?
        .canonicalize()
        .map_err(|err| {
            MarketplaceAddError::Internal(format!(
                "failed to resolve marketplace destination parent {}: {err}",
                destination.display()
            ))
        })?;
    if !destination_parent.starts_with(&install_root) {
        return Err(MarketplaceAddError::InvalidRequest(format!(
            "marketplace destination {} is outside install root {}",
            destination.display(),
            install_root.display()
        )));
    }
    Ok(())
}

pub(super) fn replace_marketplace_root(
    staged_root: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(staged_root, destination)
}

pub(super) fn marketplace_staging_root(install_root: &Path) -> PathBuf {
    install_root.join(".staging")
}
