//! On-demand storage observations and bounded, process-local eviction receipts.
//! Quotas deliberately retain their existing logical-file-length semantics.
use super::lock_retention_registry;
use super::logical_artifact_stem;
use super::metadata_is_reparse_point;
use super::normalized_retention_path;
use super::open_regular_artifact;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

const MAX_EVICTION_RECEIPTS: usize = 128;

#[derive(Clone, Copy, Debug, Serialize)]
pub(super) struct EvictionReason {
    pub(super) scope: &'static str,
    pub(super) artifact_count_limit: bool,
    pub(super) logical_storage_byte_limit: bool,
}

#[derive(Clone, Debug)]
pub(super) struct EvictionReceipt {
    path: PathBuf,
    value: Value,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct BackingId(u64, [u8; 16]);

struct Allocation {
    id: BackingId,
    bytes: u64,
    allocated: u64,
    links: u32,
}

#[derive(Default)]
struct Storage {
    logical_retained_bytes: u64,
    logical_storage_bytes: u64,
    files: u64,
    backing: BTreeMap<BackingId, (u64, u64, u32, u32)>,
}

impl Storage {
    fn add(&mut self, allocation: &Allocation, payload: bool) -> std::io::Result<()> {
        self.logical_storage_bytes = checked_add(self.logical_storage_bytes, allocation.bytes)?;
        if payload {
            self.logical_retained_bytes =
                checked_add(self.logical_retained_bytes, allocation.bytes)?;
        }
        self.files = checked_add(self.files, 1)?;
        let entry = self.backing.entry(allocation.id.clone()).or_insert((
            allocation.bytes,
            allocation.allocated,
            allocation.links,
            0,
        ));
        if (entry.0, entry.1, entry.2) != (allocation.bytes, allocation.allocated, allocation.links)
        {
            return Err(std::io::Error::other(
                "backing changed during storage observation",
            ));
        }
        entry.3 = entry
            .3
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("backing reference count overflowed"))?;
        Ok(())
    }

    fn report(&self) -> std::io::Result<Value> {
        let mut physical = 0;
        let mut shared = 0;
        let mut exclusive = 0;
        for (_, allocated, links, _) in self.backing.values() {
            physical = checked_add(physical, *allocated)?;
            if *links > 1 {
                shared = checked_add(shared, *allocated)?;
            }
            if *links == 1 {
                exclusive = checked_add(exclusive, *allocated)?;
            }
        }
        Ok(serde_json::json!({
            "logical_retained_bytes": self.logical_retained_bytes,
            "logical_storage_bytes": self.logical_storage_bytes,
            "physical_storage_bytes": physical,
            "shared_backing_physical_bytes": shared,
            "exclusive_backing_physical_bytes": exclusive,
            "file_references": self.files,
            "unique_backing_files": self.backing.len(),
        }))
    }
}

fn checked_add(left: u64, right: u64) -> std::io::Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| std::io::Error::other("storage byte total overflowed"))
}

// Query the opened object, never content hashes, path identity or file lengths
// as a proxy for physical allocation. No handles survive this observation.
#[cfg(windows)]
fn allocation(path: &Path) -> std::io::Result<Allocation> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FILE_STANDARD_INFO, FileIdInfo, FileStandardInfo,
        GetFileInformationByHandleEx,
    };
    let (file, bytes) =
        open_regular_artifact(path).map_err(|error| std::io::Error::other(error.for_model()))?;
    let mut identity = MaybeUninit::<FILE_ID_INFO>::zeroed();
    let mut standard = MaybeUninit::<FILE_STANDARD_INFO>::zeroed();
    // SAFETY: the file owns the handle and each aligned buffer has the exact
    // size/type required by its information class. Failures are not consumed.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            identity.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        ) != 0
            && GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                standard.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            ) != 0
    };
    if !success {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both successful queries initialized the complete buffers.
    let (identity, standard) = unsafe { (identity.assume_init(), standard.assume_init()) };
    if standard.AllocationSize < 0
        || standard.EndOfFile < 0
        || standard.EndOfFile as u64 != bytes
        || standard.NumberOfLinks == 0
    {
        return Err(std::io::Error::other("unstable file allocation"));
    }
    Ok(Allocation {
        id: BackingId(identity.VolumeSerialNumber, identity.FileId.Identifier),
        bytes,
        allocated: standard.AllocationSize as u64,
        links: standard.NumberOfLinks,
    })
}

#[cfg(not(windows))]
fn allocation(_path: &Path) -> std::io::Result<Allocation> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "physical allocation observation is Windows-only",
    ))
}

/// Inventory only committed artifact families, not staging files or unrelated
/// files. Physical totals deduplicate opened file IDs across the whole scope.
fn inventory(directory: &Path, artifact_id: &str, family_only: bool) -> std::io::Result<Value> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata)
            if metadata.is_dir()
                && !metadata_is_reparse_point(&metadata)
                && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::json!({"status":"absent", "thread":null, "artifact":null}));
        }
        Err(error) => return Err(error),
        Ok(_) => {
            return Err(std::io::Error::other(
                "artifact directory is not a regular directory",
            ));
        }
    }
    let mut thread = Storage::default();
    let mut artifact = Storage::default();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let stem = logical_artifact_stem(name)
            .or_else(|| name.strip_suffix(".active-tool-history"))
            .or_else(|| name.strip_suffix(".status"));
        let Some(stem) = stem else {
            continue;
        };
        if family_only && stem != artifact_id {
            continue;
        }
        let allocation = allocation(&entry.path())?;
        let payload = name.ends_with(".log") || name.contains(".segment-");
        thread.add(&allocation, payload)?;
        if stem == artifact_id {
            artifact.add(&allocation, payload)?;
        }
    }
    Ok(serde_json::json!({
        "status":"observed",
        "thread":thread.report()?,
        "artifact":artifact.report()?,
    }))
}

pub(super) fn before_eviction(path: &Path) -> Value {
    let Some((directory, id)) = path
        .parent()
        .zip(path.file_stem().and_then(|name| name.to_str()))
    else {
        return serde_json::json!({"status":"unknown"});
    };
    match inventory(directory, id, true) {
        Ok(value) => value["artifact"].clone(),
        Err(error) => serde_json::json!({"status":"unknown", "error":error.to_string()}),
    }
}

/// Called only after writer/protection admission, and never for a missing file.
/// Receipt publication is advisory; it changes neither deletion nor recovery.
pub(super) fn record_eviction(path: &Path, reason: EvictionReason, before: Value, removed: bool) {
    let path = normalized_retention_path(path);
    let id = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let value = serde_json::json!({
        "artifact_id":id,
        "reason":reason,
        "outcome":if removed {"removed"} else {"partial_or_failed"},
        "observed_at_unix_ms":std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).ok().map(|time| time.as_millis() as u64),
        "storage_before":before,
        // Last-link removal is not proof of physical release: other open
        // handles, allocation snapshots and filesystem metadata are separate.
        "physical_bytes_released":null,
        "affected_recovery_obligations":[{
            "artifact_id":id,
            "tool":"read_tool_output",
            "impact":if removed {"retained_handle_unavailable"} else {"retained_handle_may_be_incomplete"},
            "protected_history":false,
            "required_work":"unknown; consult the owning history and original selectors",
        }],
        "next_action":"Use already delivered output or inspect affected state; do not replay producer effects.",
    });
    tracing::info!(target: "codex_retention_pressure",
        thread_id = %path.parent().and_then(Path::file_name).unwrap_or_default().to_string_lossy(),
        eviction = %value, "artifact retention eviction");
    let mut registry = lock_retention_registry();
    while registry.evictions.len() >= MAX_EVICTION_RECEIPTS {
        registry.evictions.pop_front();
    }
    registry
        .evictions
        .push_back(EvictionReceipt { path, value });
}

/// An explicit audit pays for allocation queries; ordinary recovery and
/// retention reconciliation keep their existing fast listing-only path.
pub(crate) async fn audit(directory: PathBuf, artifact_id: String) -> Value {
    if artifact_id.parse::<super::ToolOutputArtifactId>().is_err() {
        return serde_json::json!({"status":"unknown", "reason":"invalid_artifact_id"});
    }
    let result = super::run_blocking_artifact_io(move || {
        let store = directory.parent().and_then(Path::file_name).and_then(|name| name.to_str()).unwrap_or("unknown");
        let path = normalized_retention_path(&directory.join(format!("{artifact_id}.log")));
        let eviction = lock_retention_registry().evictions.iter().rev()
            .find(|receipt| receipt.path == path).map(|receipt| receipt.value.clone());
        let storage = match inventory(&directory, &artifact_id, false) {
            Ok(value) => value,
            Err(error) => serde_json::json!({
                "status":"unknown", "thread":null, "artifact":null, "error":error.to_string(),
            }),
        };
        Ok(serde_json::json!({
            "storage":storage,
            "store":store,
            "scope":"caller_thread_store",
            "observation":"non_atomic_point_in_time; no retention lease",
            "physical_accounting":"unique opened volume/file IDs and allocation sizes; excludes filesystem metadata and block-level sharing",
            "logical_accounting":"retained payload file lengths; logical_storage_bytes also includes metadata/status sidecars and protection markers",
            "additivity":"thread physical total deduplicates shared backing; artifact physical totals must not be summed",
            "sharing_accounting":"shared and exclusive partition unique physical allocation by hard-link count greater than one or equal to one; sharing can extend outside this scope",
            "quota_accounting":if store == "tool-output" {"unchanged summed family file lengths including metadata; not physical allocation"}
                else if store == "command-output" {"not subject to snapshot count/byte quotas"} else {"unknown"},
            "eviction":eviction,
            "eviction_history":"last 128 receipts in this process only; absent receipt is unknown, not proof of no eviction",
        }))
    }).await;
    result
        .unwrap_or_else(|error| serde_json::json!({"status":"unknown", "error":error.to_string()}))
}
