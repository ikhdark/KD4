//! Crash recovery for effectful dispatch identities, independent of cell stores.
//! A reservation is not proof an effect happened. It prevents automatic replay
//! of that identity, even if cancellation or a crash loses the handler result.
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

const MAX_RECEIPT_BYTES: u64 = 8192;
const MAX_RECOVERY_ENTRIES: usize = 32;
const MAX_SCAN_ENTRIES: usize = 4096;

pub(crate) struct EffectReceipt {
    path: PathBuf,
    record: Value,
}

impl EffectReceipt {
    pub(crate) async fn reserve(
        directory: PathBuf,
        turn_id: String,
        call_id: String,
        tool: String,
    ) -> Result<Self, String> {
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
            let identity = serde_json::to_vec(&(&turn_id, &call_id)).map_err(|error| error.to_string())?;
            let id = format!("{:x}", Sha256::digest(identity));
            let path = directory.join(format!("{id}.json"));
            let record = json!({
                "version": 1,
                "effect_id": id,
                "turn_id": turn_id.chars().take(256).collect::<String>(),
                "call_id": call_id.chars().take(256).collect::<String>(),
                "tool": tool.chars().take(256).collect::<String>(),
                "dispatch_state": "reserved",
                "effects": "unknown",
                "automatic_replay_allowed": false,
            });
            let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let previous = read_receipt(&path).unwrap_or_else(|_| json!({
                        "dispatch_state": "receipt_unavailable", "effects": "unknown",
                    }));
                    return Err(format!(
                        "dispatch identity already reserved; no new handler was started. Inspect its existing effects instead of replaying it. Receipt {}: {previous}",
                        path.display(),
                    ));
                }
                Err(error) => return Err(format!("effect receipt reservation failed; no handler started: {error}")),
            };
            // Leave an incomplete reservation on failure: deleting it could
            // falsely tell recovery that the identity was never admitted.
            serde_json::to_writer(&mut file, &record).map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            Ok(Self { path, record })
        }).await.map_err(|error| format!("effect receipt worker failed: {error}"))?
    }

    pub(crate) async fn returned(mut self, outcome: &'static str) -> Result<(), String> {
        tokio::task::spawn_blocking(move || {
            self.record["dispatch_state"] = "handler_returned".into();
            self.record["observed_outcome"] = outcome.into();
            // A returned error or yielded process is not proof of no effects;
            // even success only establishes the handler's reported outcome.
            self.record["effects"] = "reported_not_rolled_back".into();
            let parent = self.path.parent().ok_or("effect receipt has no parent")?;
            let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
            serde_json::to_writer(staged.as_file_mut(), &self.record).map_err(|error| error.to_string())?;
            staged.as_file().sync_all().map_err(|error| error.to_string())?;
            super::command_execution::persist_synced_file(staged, &self.path, parent)
                .map_err(|error| error.to_string())
        }).await.map_err(|error| format!("effect receipt worker failed: {error}"))?
    }
}

fn read_receipt(path: &Path) -> Result<Value, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).map_err(|error| error.to_string())?
        .take(MAX_RECEIPT_BYTES + 1).read_to_end(&mut bytes).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err("oversized effect receipt".into());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if value["version"] != 1 || !matches!(value["dispatch_state"].as_str(), Some("reserved" | "handler_returned")) {
        return Err("unrecognized effect receipt".into());
    }
    Ok(value)
}

/// Read once when constructing a session's recovery context, never on every
/// model generation. Bounded scans report incompleteness rather than absence.
pub(crate) async fn recover(directory: PathBuf) -> Option<Value> {
    let location = directory.display().to_string();
    match tokio::task::spawn_blocking(move || -> Result<Option<Value>, String> {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let mut pending = Vec::new();
        let mut unknown = 0usize;
        let mut omitted = 0usize;
        let mut scan_complete = true;
        for (index, entry) in entries.enumerate() {
            if index >= MAX_SCAN_ENTRIES {
                scan_complete = false;
                break;
            }
            let entry = entry.map_err(|error| error.to_string())?;
            if entry.path().extension().is_none_or(|extension| extension != "json")
                || !entry.file_type().map_err(|error| error.to_string())?.is_file()
            {
                continue;
            }
            match read_receipt(&entry.path()) {
                Ok(record) if record["dispatch_state"] == "reserved" => {
                    if pending.len() < MAX_RECOVERY_ENTRIES {
                        pending.push(record);
                    } else {
                        omitted += 1;
                    }
                }
                Ok(_) => {}
                Err(_) => unknown += 1,
            }
        }
        if pending.is_empty() && unknown == 0 && scan_complete {
            return Ok(None);
        }
        pending.sort_by(|left, right| left["effect_id"].as_str().cmp(&right["effect_id"].as_str()));
        Ok(Some(json!({
            "journal": directory,
            "pending_dispatches": pending,
            "unreadable_receipts": unknown,
            "omitted_pending_dispatches": omitted,
            "scan_complete": scan_complete,
            "automatic_replay_allowed": false,
            "required_action": "These dispatches lack durable returned-result receipts. Effects may have occurred. Inspect the affected state or an adapter-owned receipt before any retry; missing results do not authorize replay.",
        })))
    }).await {
        Ok(Ok(value)) => value,
        failure => Some(json!({
            "journal": location,
            "scan_complete": false,
            "automatic_replay_allowed": false,
            "error": format!("effect recovery unavailable: {failure:?}"),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reservations_survive_abandonment_and_returned_effects_are_not_replayed() {
        let directory = tempfile::tempdir().unwrap();
        let reserve = || EffectReceipt::reserve(directory.path().into(), "turn".into(), "call".into(), "edit".into());
        drop(reserve().await.unwrap());
        let recovery = recover(directory.path().into()).await.unwrap();
        assert_eq!(recovery["pending_dispatches"].as_array().unwrap().len(), 1);
        assert!(reserve().await.is_err());
        let next = EffectReceipt::reserve(directory.path().into(), "turn".into(), "other".into(), "edit".into()).await.unwrap();
        next.returned("success").await.unwrap();
        assert!(EffectReceipt::reserve(directory.path().into(), "turn".into(), "other".into(), "edit".into()).await.is_err());
        assert_eq!(recover(directory.path().into()).await.unwrap()["pending_dispatches"].as_array().unwrap().len(), 1);
    }
}
