use super::MarketplaceAddError;
use super::source::MarketplaceSource;
use crate::installed_marketplaces::resolve_configured_marketplace_root;
use crate::marketplace::validate_marketplace_root;
use codex_config::CONFIG_TOML_FILE;
use codex_config::MarketplaceConfigUpdate;
use codex_config::record_user_marketplace;
use codex_utils_absolute_path::paths_match_after_normalization;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MarketplaceInstallMetadata {
    source: InstalledMarketplaceSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstalledMarketplaceSource {
    Git {
        url: String,
        ref_name: Option<String>,
        sparse_paths: Vec<String>,
    },
    Local {
        path: String,
    },
}

pub(super) fn record_added_marketplace_entry(
    codex_home: &Path,
    marketplace_name: &str,
    install_metadata: &MarketplaceInstallMetadata,
    last_revision: Option<&str>,
) -> Result<(), MarketplaceAddError> {
    let source = install_metadata.config_source();
    let timestamp = utc_timestamp_now()?;
    let update = MarketplaceConfigUpdate {
        last_updated: &timestamp,
        last_revision,
        source_type: install_metadata.config_source_type(),
        source: &source,
        ref_name: install_metadata.ref_name(),
        sparse_paths: install_metadata.sparse_paths(),
    };

    record_user_marketplace(codex_home, marketplace_name, &update).map_err(|err| {
        MarketplaceAddError::Internal(format!(
            "failed to add marketplace '{marketplace_name}' to user config.toml: {err}"
        ))
    })
}

/// Returns the root of a configured marketplace installed from this source, with the
/// revision recorded for it so re-adding the source does not discard it.
pub(super) fn installed_marketplace_root_for_source(
    codex_home: &Path,
    install_root: &Path,
    install_metadata: &MarketplaceInstallMetadata,
) -> Result<Option<(PathBuf, Option<String>)>, MarketplaceAddError> {
    let config_path = codex_home.join(CONFIG_TOML_FILE);
    let config = match fs::read_to_string(&config_path) {
        Ok(config) => config,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(MarketplaceAddError::Internal(format!(
                "failed to read user config {}: {err}",
                config_path.display()
            )));
        }
    };
    let config: toml::Value = toml::from_str(&config).map_err(|err| {
        MarketplaceAddError::Internal(format!(
            "failed to parse user config {}: {err}",
            config_path.display()
        ))
    })?;
    let Some(marketplaces) = config.get("marketplaces").and_then(toml::Value::as_table) else {
        return Ok(None);
    };

    for (marketplace_name, marketplace) in marketplaces {
        if !install_metadata.matches_config(marketplace) {
            continue;
        }
        let Some(root) =
            resolve_configured_marketplace_root(marketplace_name, marketplace, install_root)
        else {
            continue;
        };
        if validate_marketplace_root(&root).is_ok() {
            let last_revision = marketplace
                .get("last_revision")
                .and_then(toml::Value::as_str)
                .map(str::to_string);
            return Ok(Some((root, last_revision)));
        }
    }

    Ok(None)
}

pub(super) fn find_marketplace_root_by_name(
    codex_home: &Path,
    install_root: &Path,
    marketplace_name: &str,
) -> Result<Option<PathBuf>, MarketplaceAddError> {
    let config_path = codex_home.join(CONFIG_TOML_FILE);
    let config = match fs::read_to_string(&config_path) {
        Ok(config) => config,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(MarketplaceAddError::Internal(format!(
                "failed to read user config {}: {err}",
                config_path.display()
            )));
        }
    };
    let config: toml::Value = toml::from_str(&config).map_err(|err| {
        MarketplaceAddError::Internal(format!(
            "failed to parse user config {}: {err}",
            config_path.display()
        ))
    })?;
    let Some(marketplace) = config
        .get("marketplaces")
        .and_then(toml::Value::as_table)
        .and_then(|marketplaces| marketplaces.get(marketplace_name))
    else {
        return Ok(None);
    };

    let Some(root) =
        resolve_configured_marketplace_root(marketplace_name, marketplace, install_root)
    else {
        return Ok(None);
    };
    if validate_marketplace_root(&root).is_ok() {
        Ok(Some(root))
    } else {
        Ok(None)
    }
}

impl MarketplaceInstallMetadata {
    pub(super) fn from_source(source: &MarketplaceSource, sparse_paths: &[String]) -> Self {
        let source = match source {
            MarketplaceSource::Git { url, ref_name } => InstalledMarketplaceSource::Git {
                url: url.clone(),
                ref_name: ref_name.clone(),
                sparse_paths: sparse_paths.to_vec(),
            },
            MarketplaceSource::Local { path } => InstalledMarketplaceSource::Local {
                path: path.display().to_string(),
            },
        };
        Self { source }
    }

    fn config_source_type(&self) -> &'static str {
        match &self.source {
            InstalledMarketplaceSource::Git { .. } => "git",
            InstalledMarketplaceSource::Local { .. } => "local",
        }
    }

    fn config_source(&self) -> String {
        match &self.source {
            InstalledMarketplaceSource::Git { url, .. } => url.clone(),
            InstalledMarketplaceSource::Local { path } => path.clone(),
        }
    }

    fn ref_name(&self) -> Option<&str> {
        match &self.source {
            InstalledMarketplaceSource::Git { ref_name, .. } => ref_name.as_deref(),
            InstalledMarketplaceSource::Local { .. } => None,
        }
    }

    fn sparse_paths(&self) -> &[String] {
        match &self.source {
            InstalledMarketplaceSource::Git { sparse_paths, .. } => sparse_paths,
            InstalledMarketplaceSource::Local { .. } => &[],
        }
    }

    fn matches_config(&self, marketplace: &toml::Value) -> bool {
        marketplace.get("source_type").and_then(toml::Value::as_str)
            == Some(self.config_source_type())
            && marketplace
                .get("source")
                .and_then(toml::Value::as_str)
                .is_some_and(|source| match &self.source {
                    InstalledMarketplaceSource::Local { path } => {
                        paths_match_after_normalization(source, path)
                    }
                    InstalledMarketplaceSource::Git { url, .. } => source == url,
                })
            && match marketplace.get("ref") {
                None => self.ref_name().is_none(),
                Some(value) => value
                    .as_str()
                    .is_some_and(|value| Some(value) == self.ref_name()),
            }
            && config_sparse_paths(marketplace).as_deref() == Some(self.sparse_paths())
    }
}

fn config_sparse_paths(marketplace: &toml::Value) -> Option<Vec<String>> {
    match marketplace.get("sparse_paths") {
        None => Some(Vec::new()),
        Some(paths) => paths
            .as_array()?
            .iter()
            .map(|path| path.as_str().map(str::to_string))
            .collect(),
    }
}

fn utc_timestamp_now() -> Result<String, MarketplaceAddError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| {
            MarketplaceAddError::Internal(format!("system clock is before Unix epoch: {err}"))
        })?;
    Ok(format_utc_timestamp(duration.as_secs() as i64))
}

fn format_utc_timestamp(seconds_since_epoch: i64) -> String {
    const SECONDS_PER_DAY: i64 = 86_400;
    let days = seconds_since_epoch.div_euclid(SECONDS_PER_DAY);
    let seconds_of_day = seconds_since_epoch.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}
