//! Private on-disk indirection; public rollout readers still return full records.
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use sha2::{Digest, Sha256};

const MAX_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const INLINE_BYTES: usize = 8 * 1024;
const KIND: &str = "rollout_payload_artifact";

pub(crate) fn root(path: &Path) -> PathBuf {
    for ancestor in path.ancestors().skip(1) {
        if ancestor.file_name().is_some_and(|name|
            name == crate::SESSIONS_SUBDIR || name == crate::ARCHIVED_SESSIONS_SUBDIR)
        {
            return ancestor.parent().unwrap_or(ancestor).join("rollout-payloads");
        }
    }
    path.parent().unwrap_or(Path::new(".")).join("rollout-payloads")
}

fn digest(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

/// Artifacts are immutable and durable before the referencing line is appended.
/// Failures fall back to the original inline record at the writer boundary.
pub(crate) fn store_line(path: &Path, line: &[u8]) -> io::Result<Vec<u8>> {
    if line.len() < INLINE_BYTES { return Ok(line.to_vec()); }
    let mut item: serde_json::Value = serde_json::from_slice(line)?;
    // Full tool-manifest definitions are usually byte-identical across threads,
    // so the content address stores one copy for every rollout that uses them.
    let selected = item["type"] == "session_meta"
        || item["type"] == "tool_manifest"
        || (item["type"] == "sampling_boundary" && item["payload"]["timing_checkpoint"].is_object())
        || (item["type"] == "event_msg" && matches!(item["payload"]["type"].as_str(),
            Some("patch_apply_end" | "task_complete" | "turn_aborted")));
    if !selected { return Ok(line.to_vec()); }
    let fields = item.as_object_mut().ok_or_else(|| io::Error::other("invalid rollout object"))?;
    let timestamp = fields.remove("timestamp");
    let format_version = fields.remove("format_version");
    let bytes = serde_json::to_vec(&item)?;
    if bytes.len() as u64 > MAX_PAYLOAD_BYTES { return Ok(line.to_vec()); }
    let sha256 = digest(&bytes);
    let directory = root(path);
    std::fs::create_dir_all(&directory)?;
    let destination = directory.join(format!("{sha256}.json"));
    let mut published_here = false;
    if !destination.exists() {
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&destination) {
            // The destination is exactly the fsynced temporary file we wrote.
            Ok(_) => published_here = true,
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.error),
        }
    }
    // A preexisting object or a concurrent writer that won admission is
    // verified: never reference a partial, corrupted, or substituted object,
    // including a preexisting symlink.
    if !published_here {
        read_payload(&directory, &sha256, bytes.len() as u64)?;
    }
    let mut reference = serde_json::json!({
        "timestamp": timestamp, "format_version": format_version,
        "type": KIND, "payload": {"sha256": sha256, "bytes": bytes.len()}
    });
    reference["payload"]["item_type"] = item["type"].clone();
    let mut serialized = serde_json::to_vec(&reference)?;
    serialized.push(b'\n');
    Ok(serialized)
}

fn read_payload(directory: &Path, sha256: &str, bytes: u64) -> io::Result<Vec<u8>> {
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || bytes > MAX_PAYLOAD_BYTES
    {
        return Err(io::Error::other("invalid rollout payload reference"));
    }
    let path = directory.join(format!("{sha256}.json"));
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() || metadata.len() != bytes {
        return Err(io::Error::other("rollout payload is missing, replaced, or incomplete"));
    }
    let mut payload = Vec::new();
    std::fs::File::open(path)?.take(bytes.saturating_add(1)).read_to_end(&mut payload)?;
    if payload.len() as u64 != bytes || digest(&payload) != sha256 {
        return Err(io::Error::other("rollout payload checksum mismatch"));
    }
    Ok(payload)
}

pub(crate) fn hydrate_line(path: &Path, line: String) -> io::Result<String> {
    if !line.contains(KIND) { return Ok(line); }
    let reference: serde_json::Value = match serde_json::from_str(&line) {
        Ok(value) => value,
        Err(_) => return Ok(line), // Preserve the existing torn-line handling.
    };
    if reference["type"] != KIND { return Ok(line); }
    let sha256 = reference["payload"]["sha256"].as_str()
        .ok_or_else(|| io::Error::other("rollout payload has no hash"))?;
    let bytes = reference["payload"]["bytes"].as_u64()
        .ok_or_else(|| io::Error::other("rollout payload has no byte length"))?;
    let mut item: serde_json::Value = serde_json::from_slice(&read_payload(&root(path), sha256, bytes)?)?;
    if item["type"] != reference["payload"]["item_type"] || item["type"] == KIND {
        return Err(io::Error::other("rollout payload type mismatch"));
    }
    item["timestamp"] = reference["timestamp"].clone();
    item["format_version"] = reference["format_version"].clone();
    serde_json::to_string(&item).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_reader_restores_payloads_and_rejects_corrupted_undo_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let item = serde_json::json!({
            "timestamp": "2026-10-02T00:00:00Z", "format_version": 1,
            "type": "event_msg", "payload": {"type": "patch_apply_end",
                "call_id": "delete", "changes": {"file.txt": {
                    "type": "delete", "content": "saved text\n".repeat(2000)
                }}}
        });
        let stored = store_line(&path, &serde_json::to_vec(&item).unwrap()).unwrap();
        let reference: serde_json::Value = serde_json::from_slice(&stored).unwrap();
        assert_eq!(reference["type"], KIND);
        std::fs::write(&path, stored).unwrap();
        let mut reader = crate::compression::open_rollout_line_reader(&path).await.unwrap();
        let restored: serde_json::Value = serde_json::from_str(
            &reader.next_line().await.unwrap().unwrap(),
        ).unwrap();
        assert_eq!(restored, item);
        let sha256 = reference["payload"]["sha256"].as_str().unwrap();
        std::fs::write(root(&path).join(format!("{sha256}.json")), b"corrupt").unwrap();
        let mut reader = crate::compression::open_rollout_line_reader(&path).await.unwrap();
        assert!(reader.next_line().await.is_err());
    }
}
