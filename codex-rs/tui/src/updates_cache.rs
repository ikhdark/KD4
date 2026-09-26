use crate::legacy_core::config::Config;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct VersionInfo {
    pub(crate) latest_version: String,
    // ISO-8601 timestamp (RFC3339)
    pub(crate) last_checked_at: DateTime<Utc>,
    #[serde(default)]
    pub(crate) dismissed_version: Option<String>,
}

const VERSION_FILENAME: &str = "version.json";

pub(crate) fn version_filepath(config: &Config) -> PathBuf {
    config.codex_home.join(VERSION_FILENAME).into_path_buf()
}

#[cfg(test)]
pub(crate) fn read_version_info(version_file: &Path) -> anyhow::Result<VersionInfo> {
    let contents = std::fs::read_to_string(version_file)?;
    Ok(serde_json::from_str(&contents)?)
}

pub(crate) async fn read_version_info_async(version_file: &Path) -> anyhow::Result<VersionInfo> {
    let contents = tokio::fs::read_to_string(version_file).await?;
    Ok(serde_json::from_str(&contents)?)
}

/// Persist a dismissal for the current latest version so we don't show
/// the update popup again for this version.
pub(crate) async fn dismiss_version(config: &Config, version: &str) -> anyhow::Result<()> {
    let version = version.to_string();
    merge_version_info(version_filepath(config), version.clone(), move |info| {
        info.dismissed_version = Some(version);
    })
    .await
}

pub(crate) async fn cache_release(
    version_file: &Path,
    latest_version: String,
) -> anyhow::Result<()> {
    merge_version_info(
        version_file.to_path_buf(),
        latest_version.clone(),
        move |info| {
            info.latest_version = latest_version;
            info.last_checked_at = Utc::now();
        },
    )
    .await
}

async fn merge_version_info(
    version_file: PathBuf,
    fallback_version: String,
    update: impl FnOnce(&mut VersionInfo) + Send + 'static,
) -> anyhow::Result<()> {
    // Keep the lock and the whole transaction on one worker. Cancellation of the
    // async caller cannot release the lock before the replacement finishes.
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let _lock = codex_file_system::acquire_atomic_write_lock(&version_file)?;
        let previous = match std::fs::read(&version_file) {
            Ok(contents) => serde_json::from_slice(&contents).ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut info = previous.unwrap_or(VersionInfo {
            latest_version: fallback_version,
            last_checked_at: DateTime::<Utc>::UNIX_EPOCH,
            dismissed_version: None,
        });
        update(&mut info);
        let json_line = format!("{}\n", serde_json::to_string(&info)?);
        codex_file_system::write_atomically(&version_file, &json_line)?;
        Ok(())
    })
    .await?
}

#[cfg(test)]
#[path = "updates_cache_tests.rs"]
mod tests;
