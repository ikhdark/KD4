use crate::installed_marketplaces::marketplace_install_root;
use crate::marketplace_policy::validate_marketplace_name_for_add;
use crate::marketplace_policy::validate_marketplace_source_for_add;
use crate::marketplace_upgrade::write_added_git_marketplace_metadata;
use codex_config::ConfigRequirements;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use tempfile::Builder;

mod install;
mod metadata;
mod source;

use install::clone_git_source;
use install::ensure_marketplace_destination_is_inside_install_root;
use install::marketplace_staging_root;
use install::replace_marketplace_root;
use install::safe_marketplace_dir_name;
use metadata::MarketplaceInstallMetadata;
use metadata::find_marketplace_root_by_name;
use metadata::installed_marketplace_root_for_source;
use metadata::record_added_marketplace_entry;
pub(crate) use source::MarketplaceSource;
pub(crate) use source::canonicalize_configured_git_source;
pub(crate) use source::normalize_git_url;
pub(crate) use source::normalize_github_shorthand_url;
pub(crate) use source::parse_marketplace_source;
use source::stage_marketplace_source;
use source::validate_marketplace_source_root;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceAddRequest {
    pub source: String,
    pub ref_name: Option<String>,
    pub sparse_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceAddOutcome {
    pub marketplace_name: String,
    pub source_display: String,
    pub installed_root: AbsolutePathBuf,
    pub already_added: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MarketplaceAddError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("{0}")]
    Internal(String),
}

pub async fn add_marketplace(
    codex_home: PathBuf,
    requirements: ConfigRequirements,
    request: MarketplaceAddRequest,
) -> Result<MarketplaceAddOutcome, MarketplaceAddError> {
    tokio::task::spawn_blocking(move || {
        add_marketplace_sync(codex_home.as_path(), &requirements, request)
    })
    .await
    .map_err(|err| MarketplaceAddError::Internal(format!("failed to add marketplace: {err}")))?
}

pub fn is_local_marketplace_source(
    source: &str,
    explicit_ref: Option<String>,
) -> Result<bool, MarketplaceAddError> {
    Ok(matches!(
        parse_marketplace_source(source, explicit_ref)?,
        source::MarketplaceSource::Local { .. }
    ))
}

fn add_marketplace_sync(
    codex_home: &Path,
    requirements: &ConfigRequirements,
    request: MarketplaceAddRequest,
) -> Result<MarketplaceAddOutcome, MarketplaceAddError> {
    add_marketplace_sync_with_cloner(codex_home, requirements, request, clone_git_source)
}

fn add_marketplace_sync_with_cloner<F>(
    codex_home: &Path,
    requirements: &ConfigRequirements,
    request: MarketplaceAddRequest,
    clone_source: F,
) -> Result<MarketplaceAddOutcome, MarketplaceAddError>
where
    F: Fn(&str, Option<&str>, &[String], &Path) -> Result<String, MarketplaceAddError>,
{
    let MarketplaceAddRequest {
        source,
        ref_name,
        sparse_paths,
    } = request;
    let source = parse_marketplace_source(&source, ref_name)?;
    let managed_marketplace_name =
        validate_marketplace_source_for_add(codex_home, requirements, &source)
            .map_err(MarketplaceAddError::InvalidRequest)?;
    if !sparse_paths.is_empty() && !matches!(source, MarketplaceSource::Git { .. }) {
        return Err(MarketplaceAddError::InvalidRequest(
            "--sparse is only supported for git marketplace sources".to_string(),
        ));
    }

    let install_root = marketplace_install_root(codex_home);
    fs::create_dir_all(&install_root).map_err(|err| {
        MarketplaceAddError::Internal(format!(
            "failed to create marketplace install directory {}: {err}",
            install_root.display()
        ))
    })?;

    let install_metadata = MarketplaceInstallMetadata::from_source(&source, &sparse_paths);
    if let Some((existing_root, _)) =
        installed_marketplace_root_for_source(codex_home, &install_root, &install_metadata)?
    {
        let marketplace_name = validate_marketplace_source_root(&existing_root)?;
        validate_marketplace_name_for_add(managed_marketplace_name, &marketplace_name)
            .map_err(MarketplaceAddError::InvalidRequest)?;
        let _operation = crate::installed_marketplaces::try_lock_marketplace_operation(codex_home, &marketplace_name)
            .map_err(|err| MarketplaceAddError::Internal(format!("marketplace operation is busy or unavailable: {err}")))?;
        // The optimistic source lookup is not publication authority. Re-read
        // source, root and revision under the same lease used by remove/upgrade.
        let (existing_root, last_revision) = installed_marketplace_root_for_source(codex_home, &install_root, &install_metadata)?
            .ok_or_else(|| MarketplaceAddError::InvalidRequest("marketplace changed during add; retry the request".to_string()))?;
        if validate_marketplace_source_root(&existing_root)? != marketplace_name {
            return Err(MarketplaceAddError::InvalidRequest("marketplace changed during add; retry the request".to_string()));
        }
        record_added_marketplace_entry(
            codex_home,
            &marketplace_name,
            &install_metadata,
            last_revision.as_deref(),
        )?;
        return Ok(MarketplaceAddOutcome {
            marketplace_name,
            source_display: source.display(),
            installed_root: AbsolutePathBuf::try_from(existing_root).map_err(|err| {
                MarketplaceAddError::Internal(format!(
                    "failed to resolve installed marketplace root: {err}"
                ))
            })?,
            already_added: true,
        });
    }

    if let MarketplaceSource::Local { path } = &source {
        let marketplace_name = validate_marketplace_source_root(path)?;
        validate_marketplace_name_for_add(managed_marketplace_name, &marketplace_name)
            .map_err(MarketplaceAddError::InvalidRequest)?;
        let _operation = crate::installed_marketplaces::try_lock_marketplace_operation(codex_home, &marketplace_name)
            .map_err(|err| MarketplaceAddError::Internal(format!("marketplace operation is busy or unavailable: {err}")))?;
        if validate_marketplace_source_root(path)? != marketplace_name {
            return Err(MarketplaceAddError::InvalidRequest("marketplace changed during add; retry the request".to_string()));
        }
        if find_marketplace_root_by_name(codex_home, &install_root, &marketplace_name)?.is_some() {
            return Err(MarketplaceAddError::InvalidRequest(format!(
                "marketplace '{marketplace_name}' is already added from a different source; remove it before adding this source"
            )));
        }
        record_added_marketplace_entry(
            codex_home,
            &marketplace_name,
            &install_metadata,
            /*last_revision*/ None,
        )?;
        return Ok(MarketplaceAddOutcome {
            marketplace_name,
            source_display: source.display(),
            installed_root: AbsolutePathBuf::try_from(path.clone()).map_err(|err| {
                MarketplaceAddError::Internal(format!(
                    "failed to resolve installed marketplace root: {err}"
                ))
            })?,
            already_added: false,
        });
    }

    let staging_root = marketplace_staging_root(&install_root);
    fs::create_dir_all(&staging_root).map_err(|err| {
        MarketplaceAddError::Internal(format!(
            "failed to create marketplace staging directory {}: {err}",
            staging_root.display()
        ))
    })?;
    let staged_dir = Builder::new()
        .prefix("marketplace-add-")
        .tempdir_in(&staging_root)
        .map_err(|err| {
            MarketplaceAddError::Internal(format!(
                "failed to create temporary marketplace directory in {}: {err}",
                staging_root.display()
            ))
        })?;
    let staged_root = staged_dir.path().to_path_buf();

    let revision = stage_marketplace_source(&source, &sparse_paths, &staged_root, clone_source)?;

    let marketplace_name = validate_marketplace_source_root(&staged_root)?;
    validate_marketplace_name_for_add(managed_marketplace_name, &marketplace_name)
        .map_err(MarketplaceAddError::InvalidRequest)?;
    if let MarketplaceSource::Git { url, ref_name } = &source {
        write_added_git_marketplace_metadata(
            &staged_root,
            &marketplace_name,
            url,
            ref_name.as_deref(),
            &sparse_paths,
            &revision,
        )
        .map_err(MarketplaceAddError::Internal)?;
    }

    let destination = install_root.join(safe_marketplace_dir_name(&marketplace_name)?);
    ensure_marketplace_destination_is_inside_install_root(&install_root, &destination)?;
    // Downloads remain private and independent. Serialize only this name's
    // final revalidation/publication/config transaction and its rollback.
    let _operation = crate::installed_marketplaces::try_lock_marketplace_operation(codex_home, &marketplace_name)
        .map_err(|err| MarketplaceAddError::Internal(format!("marketplace operation is busy or unavailable: {err}")))?;
    if destination.exists()
        || find_marketplace_root_by_name(codex_home, &install_root, &marketplace_name)?.is_some()
    {
        return Err(MarketplaceAddError::InvalidRequest(format!(
            "marketplace '{marketplace_name}' is already added from a different source; remove it before adding this source"
        )));
    }
    replace_marketplace_root(&staged_root, &destination).map_err(|err| {
        MarketplaceAddError::Internal(format!(
            "failed to install marketplace at {}: {err}",
            destination.display()
        ))
    })?;
    if let Err(err) = record_added_marketplace_entry(
        codex_home,
        &marketplace_name,
        &install_metadata,
        Some(&revision),
    ) {
        if let Err(rollback_err) = fs::rename(&destination, &staged_root) {
            return Err(MarketplaceAddError::Internal(format!(
                "{err}; additionally failed to roll back installed marketplace at {}: {rollback_err}",
                destination.display()
            )));
        }
        return Err(err);
    }

    Ok(MarketplaceAddOutcome {
        marketplace_name,
        source_display: source.display(),
        installed_root: AbsolutePathBuf::try_from(destination).map_err(|err| {
            MarketplaceAddError::Internal(format!(
                "failed to resolve installed marketplace root: {err}"
            ))
        })?,
        already_added: false,
    })
}
