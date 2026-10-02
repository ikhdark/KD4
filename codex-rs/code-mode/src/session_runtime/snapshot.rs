use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tempfile::NamedTempFile;

use crate::runtime::MAX_SESSION_STORED_VALUE_BYTES;
use crate::runtime::StoredValue;
use crate::runtime::stored_values_with_writes_within_limits;
use super::CellEvent;

const MAX_SNAPSHOT_BYTES: usize =
    MAX_SESSION_STORED_VALUE_BYTES + crate::runtime::MAX_BUFFERED_OUTPUT_BYTES + 128 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot<V> {
    version: u32,
    revision: u64,
    completed_call_id: String,
    values: BTreeMap<String, V>,
    #[serde(default)]
    completed_cells: BTreeMap<String, CellEvent>,
    #[serde(default)]
    next_cell_id: u64,
}

/// One thread owns the snapshot for the lifetime of its runtime. A replacement
/// host may acquire it after a crash; it restores values, never live cells.
pub(super) struct DurableState {
    pub(super) path: PathBuf,
    _lease: File,
    revision: AtomicU64,
    completed_cells: Mutex<BTreeMap<String, CellEvent>>,
    pub(super) first_cell_id: u64,
    pub(super) cell_id_limit: u64,
}

pub(super) struct StagedSnapshot {
    file: NamedTempFile,
    revision: u64,
    completed_cells: BTreeMap<String, CellEvent>,
}

impl DurableState {
    pub(super) fn open(path: PathBuf) -> Result<(Self, HashMap<String, StoredValue>), String> {
        if !path.is_absolute() {
            return Err("named-state snapshot path must be host-selected and absolute".into());
        }
        let parent = path.parent().ok_or("snapshot path has no parent")?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let lease = OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(path.with_extension("lock")).map_err(|error| error.to_string())?;
        fs2::FileExt::try_lock_exclusive(&lease)
            .map_err(|error| format!("named-state snapshot is owned by another runtime: {error}"))?;
        let snapshot = match File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take((MAX_SNAPSHOT_BYTES + 1) as u64).read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
                if bytes.len() > MAX_SNAPSHOT_BYTES {
                    return Err("named-state snapshot exceeds its byte limit".into());
                }
                let snapshot: Snapshot<Value> = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("named-state snapshot is invalid; no cell started: {error}"))?;
                if !matches!(snapshot.version, 1..=3) {
                    return Err("unsupported named-state snapshot version; no cell started".into());
                }
                if snapshot.completed_cells.len() > super::TERMINAL_CELL_CACHE_CAPACITY
                    || snapshot.completed_cells.iter().any(|(id, event)| {
                        id.parse::<u64>().is_err()
                            || !matches!(event, CellEvent::Completed { .. })
                    })
                {
                    return Err("invalid completed-cell snapshot; no cell started".into());
                }
                snapshot
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Snapshot {
                version: 1,
                revision: 0,
                completed_call_id: String::new(),
                values: BTreeMap::new(),
                completed_cells: BTreeMap::new(),
                next_cell_id: 1,
            },
            Err(error) => return Err(error.to_string()),
        };
        let values = snapshot.values.into_iter().map(|(key, value)| {
            let stored = StoredValue::new(&key, value);
            (key, stored)
        }).collect::<HashMap<_, _>>();
        if !stored_values_with_writes_within_limits(&HashMap::new(), &values) {
            return Err("named-state snapshot exceeds the session storage limit".into());
        }
        let first_cell_id = snapshot.completed_cells.keys()
            .filter_map(|id| id.parse::<u64>().ok()).max().unwrap_or(0)
            .checked_add(1).ok_or("durable cell ID space exhausted")?
            .max(snapshot.next_cell_id);
        // Reserve a disjoint range before any cell starts. Interrupted cells
        // have uncertain effects and must never alias a new cell after restart.
        let cell_id_limit = first_cell_id.checked_add(1u64 << 32)
            .ok_or("durable cell ID space exhausted")?;
        let state = Self {
            path,
            _lease: lease,
            revision: AtomicU64::new(snapshot.revision),
            completed_cells: Mutex::new(snapshot.completed_cells),
            first_cell_id,
            cell_id_limit,
        };
        let reservation = state.stage_state(
            snapshot.completed_call_id, values.clone(), state.completed_cells(),
        )?;
        state.publish(reservation)?;
        Ok((state, values))
    }

    pub(super) fn completed_cells(&self) -> BTreeMap<String, CellEvent> {
        self.completed_cells.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Serialize and synchronize off the async runtime, before entering the
    /// cell's commit barrier. Dropping this file never changes the snapshot.
    pub(super) fn stage(
        &self,
        completed_call_id: String,
        values: HashMap<String, StoredValue>,
        cell_id: String,
        event: CellEvent,
    ) -> Result<StagedSnapshot, String> {
        let mut completed_cells = self.completed_cells();
        completed_cells.insert(cell_id.clone(), event);
        // Preserve the newest result exactly, including its artifact references.
        // Evict older receipts rather than truncating recovery information.
        while completed_cells.len() > super::TERMINAL_CELL_CACHE_CAPACITY
            || (completed_cells.len() > 1
                && completed_cells.values().map(super::cell_event_bytes).sum::<usize>()
                    > super::TERMINAL_CELL_CACHE_MAX_BYTES)
        {
            let oldest = completed_cells.keys().filter(|id| id.as_str() != cell_id.as_str())
                .min_by_key(|id| id.parse::<u64>().unwrap_or(u64::MAX)).cloned();
            if let Some(oldest) = oldest {
                completed_cells.remove(&oldest);
            } else {
                break;
            }
        }
        self.stage_state(completed_call_id, values, completed_cells)
    }

    fn stage_state(
        &self,
        completed_call_id: String,
        values: HashMap<String, StoredValue>,
        completed_cells: BTreeMap<String, CellEvent>,
    ) -> Result<StagedSnapshot, String> {
        let revision = self.revision.load(Ordering::Acquire).checked_add(1)
            .ok_or("named-state revision exhausted")?;
        let snapshot = Snapshot {
            version: 3,
            revision,
            completed_call_id,
            values: values.into_iter().map(|(key, value)| (key, Arc::clone(&value.value)))
                .collect::<BTreeMap<_, _>>(),
            completed_cells: completed_cells.clone(),
            next_cell_id: self.cell_id_limit,
        };
        let mut file = NamedTempFile::new_in(self.path.parent().ok_or("snapshot has no parent")?)
            .map_err(|error| error.to_string())?;
        serde_json::to_writer(file.as_file_mut(), &snapshot).map_err(|error| error.to_string())?;
        if file.as_file().metadata().map_err(|error| error.to_string())?.len() > MAX_SNAPSHOT_BYTES as u64 {
            return Err("named-state snapshot exceeds its byte limit".into());
        }
        file.as_file().sync_all().map_err(|error| error.to_string())?;
        Ok(StagedSnapshot { file, revision, completed_cells })
    }

    /// Called at the same linearization point as the in-memory commit. This is
    /// process-crash recovery, not a promise of power-loss durability on every OS.
    pub(super) fn publish(&self, staged: StagedSnapshot) -> Result<(), String> {
        let StagedSnapshot { file, revision, completed_cells } = staged;
        file.persist(&self.path).map_err(|error| error.to_string())?;
        *self.completed_cells.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            completed_cells;
        self.revision.store(revision, Ordering::Release);
        Ok(())
    }
}
