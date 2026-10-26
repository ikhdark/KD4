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

use super::CellEvent;
use crate::runtime::MAX_SESSION_STORED_VALUE_BYTES;
use crate::runtime::StoredValue;
use crate::runtime::stored_values_with_writes_within_limits;

const MAX_SNAPSHOT_BYTES: usize =
    MAX_SESSION_STORED_VALUE_BYTES + crate::runtime::MAX_BUFFERED_OUTPUT_BYTES + 128 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "V: Deserialize<'de>, E: Deserialize<'de>, P: Deserialize<'de>"))]
struct Snapshot<V, E = CellEvent, P = Value> {
    version: u32,
    revision: u64,
    completed_call_id: String,
    values: BTreeMap<String, V>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    presentations: BTreeMap<String, P>,
    #[serde(default)]
    completed_cells: BTreeMap<String, E>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    completed_cell_order: Vec<String>,
    #[serde(default)]
    next_cell_id: u64,
}

/// One thread owns the snapshot for the lifetime of its runtime. A replacement
/// host may acquire it after a crash; it restores values, never live cells.
pub(super) struct DurableState {
    pub(super) path: PathBuf,
    _lease: File,
    revision: AtomicU64,
    completed_cells: Mutex<BTreeMap<String, Arc<CompletedSnapshotEvent>>>,
    pub(super) first_cell_id: u64,
    pub(super) cell_id_limit: u64,
}

pub(super) struct StagedSnapshot {
    file: NamedTempFile,
    revision: u64,
    completed_cells: BTreeMap<String, Arc<CompletedSnapshotEvent>>,
}

// Immutable terminal payloads belong to this snapshot owner. Staging another
// cell shares their wire bytes rather than cloning and serializing every old
// receipt again. Publication still atomically replaces one complete snapshot.
pub(super) struct CompletedSnapshotEvent {
    event: CellEvent,
    serialized: Arc<serde_json::value::RawValue>,
    bytes: usize,
    completion_order: u64,
}

impl CompletedSnapshotEvent {
    pub(super) fn read(&self) -> CellEvent {
        self.event.clone()
    }

    pub(super) fn completion(&self) -> Option<bool> {
        super::terminal_completion(&self.event)
    }

    fn new(event: CellEvent, completion_order: u64) -> Result<Arc<Self>, String> {
        let serialized =
            serde_json::value::to_raw_value(&event).map_err(|error| error.to_string())?;
        let bytes = super::cell_event_bytes_with_encoded_len(&event, serialized.get().len());
        Ok(Arc::new(Self {
            event,
            serialized: Arc::from(serialized),
            bytes,
            completion_order,
        }))
    }
}

impl DurableState {

    pub(super) fn open_with_values(
        path: PathBuf,
        initial_values: HashMap<String, StoredValue>,
        minimum_cell_id: u64,
    ) -> Result<(Self, HashMap<String, StoredValue>), String> {
        if !path.is_absolute() {
            return Err("named-state snapshot path must be host-selected and absolute".into());
        }
        let parent = path.parent().ok_or("snapshot path has no parent")?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let lease = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))
            .map_err(|error| error.to_string())?;
        fs2::FileExt::try_lock_exclusive(&lease).map_err(|error| {
            format!("named-state snapshot is owned by another runtime: {error}")
        })?;
        if parent
            .file_name()
            .is_some_and(|name| name == "code-mode-state")
        {
            let home = parent.parent().ok_or("snapshot has no home")?;
            let id = path.file_stem().ok_or("snapshot has no thread identity")?;
            if home
                .join("tool-history")
                .join(id)
                .with_extension("retired")
                .try_exists()
                .map_err(|error| error.to_string())?
            {
                return Err("thread storage was permanently retired".into());
            }
        }
        let snapshot = match File::open(&path) {
            Ok(file) => {
                if !initial_values.is_empty() {
                    return Err(
                        "existing named-state snapshot cannot replace in-memory values".into(),
                    );
                }
                let mut bytes = Vec::new();
                file.take((MAX_SNAPSHOT_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
                if bytes.len() > MAX_SNAPSHOT_BYTES {
                    return Err("named-state snapshot exceeds its byte limit".into());
                }
                // Raw payloads do not spend their standalone recursion budget
                // on the snapshot envelope. Decode each value below using the
                // same bounded parser as store(), never disable depth checks.
                let snapshot: Snapshot<Box<serde_json::value::RawValue>> =
                    serde_json::from_slice(&bytes).map_err(|error| {
                        format!("named-state snapshot is invalid; no cell started: {error}")
                    })?;
                if !matches!(snapshot.version, 1..=7) {
                    return Err("unsupported named-state snapshot version; no cell started".into());
                }
                if snapshot.completed_cells.len() > super::TERMINAL_CELL_CACHE_CAPACITY
                    || snapshot.completed_cells.iter().any(|(id, event)| {
                        super::cell_id_sequence(id).is_none()
                            || !matches!(
                                event,
                                CellEvent::Completed { .. } | CellEvent::Terminated { .. }
                            )
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
                presentations: BTreeMap::new(),
                completed_cells: BTreeMap::new(),
                completed_cell_order: Vec::new(),
                next_cell_id: 1,
            },
            Err(error) => return Err(error.to_string()),
        };
        let mut completion_order = snapshot.completed_cell_order;
        if snapshot.version < 6 && completion_order.is_empty() {
            // Legacy snapshots did not record completion chronology. Preserve
            // their allocation-order fallback, never infer order from map keys.
            completion_order = snapshot.completed_cells.keys().cloned().collect();
            completion_order.sort_by_key(|id| super::cell_id_sequence(id).unwrap_or(u64::MAX));
        }
        let order_len = completion_order.len();
        let order = completion_order
            .into_iter()
            .enumerate()
            .map(|(index, id)| (id, index as u64))
            .collect::<BTreeMap<_, _>>();
        if order.len() != order_len
            || order.len() != snapshot.completed_cells.len()
            || order.keys().ne(snapshot.completed_cells.keys())
        {
            return Err("invalid completed-cell order; no cell started".into());
        }
        let mut values = snapshot
            .values
            .into_iter()
            .map(|(key, value)| {
                let value = serde_json::from_str(value.get()).map_err(|error| {
                    format!("named-state snapshot value {key:?} is invalid; no cell started: {error}")
                })?;
                let stored = StoredValue::new(&key, value)
                    .with_presentation(snapshot.presentations.get(&key).cloned());
                Ok((key, stored))
            })
            .collect::<Result<HashMap<_, _>, String>>()?;
        values.extend(initial_values);
        if !stored_values_with_writes_within_limits(&HashMap::new(), &values) {
            return Err("named-state snapshot exceeds the session storage limit".into());
        }
        let first_cell_id = snapshot
            .completed_cells
            .keys()
            .filter_map(|id| super::cell_id_sequence(id))
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("durable cell ID space exhausted")?
            .max(snapshot.next_cell_id)
            .max(minimum_cell_id);
        // Reserve a disjoint range before any cell starts. Interrupted cells
        // have uncertain effects and must never alias a new cell after restart.
        let cell_id_limit = first_cell_id
            .checked_add(1u64 << 32)
            .ok_or("durable cell ID space exhausted")?;
        let state = Self {
            path,
            _lease: lease,
            revision: AtomicU64::new(snapshot.revision),
            completed_cells: Mutex::new(
                snapshot
                    .completed_cells
                    .into_iter()
                    .map(|(id, event)| {
                        CompletedSnapshotEvent::new(event, order[&id]).map(|event| (id, event))
                    })
                    .collect::<Result<_, _>>()?,
            ),
            first_cell_id,
            cell_id_limit,
        };
        let reservation = state.stage_state(
            snapshot.completed_call_id,
            values.clone(),
            state
                .completed_cells
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )?;
        state.publish(reservation)?;
        Ok((state, values))
    }

    pub(super) fn completed_cells_for_replay(
        &self,
    ) -> Vec<(String, Arc<CompletedSnapshotEvent>, usize)> {
        let cells = self
            .completed_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ordered = cells.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(_, event)| event.completion_order);
        ordered
            .into_iter()
            .map(|(id, event)| (id.clone(), Arc::clone(event), event.bytes))
            .collect()
    }

    pub(super) fn completed_cell_for_replay(
        &self,
        cell_id: &str,
    ) -> Option<(Arc<CompletedSnapshotEvent>, usize)> {
        self.completed_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell_id)
            .map(|event| (Arc::clone(event), event.bytes))
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
        let mut completed_cells = self
            .completed_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let completion_order = completed_cells
            .values()
            .map(|event| event.completion_order)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("completion order exhausted")?;
        completed_cells.insert(
            cell_id.clone(),
            CompletedSnapshotEvent::new(event, completion_order)?,
        );
        // Preserve the newest result exactly, including its artifact references.
        // Evict older receipts rather than truncating recovery information.
        while completed_cells.len() > super::TERMINAL_CELL_CACHE_CAPACITY
            || (completed_cells.len() > 1
                && completed_cells
                    .values()
                    .map(|event| event.bytes)
                    .sum::<usize>()
                    > super::TERMINAL_CELL_CACHE_MAX_BYTES)
        {
            let oldest = completed_cells
                .iter()
                .filter(|(id, _)| id.as_str() != cell_id.as_str())
                .min_by_key(|(_, event)| event.completion_order)
                .map(|(id, _)| id.clone());
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
        completed_cells: BTreeMap<String, Arc<CompletedSnapshotEvent>>,
    ) -> Result<StagedSnapshot, String> {
        let revision = self
            .revision
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or("named-state revision exhausted")?;
        let mut completed_cell_order = completed_cells.iter().collect::<Vec<_>>();
        completed_cell_order.sort_by_key(|(_, event)| event.completion_order);
        let completed_cell_order = completed_cell_order
            .into_iter()
            .map(|(id, _)| id.clone())
            .collect();
        let snapshot = Snapshot {
            version: 7,
            revision,
            completed_call_id,
            presentations: values
                .iter()
                .filter_map(|(key, value)| {
                    value
                        .presentation
                        .as_ref()
                        .map(|metadata| (key.clone(), Arc::clone(metadata)))
                })
                .collect(),
            values: values
                .into_iter()
                .map(|(key, value)| {
                    value
                        .serialized
                        .map(|json| (key, json))
                        .ok_or_else(|| "stored value has no serialized representation".to_string())
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?,
            completed_cells: completed_cells
                .iter()
                .map(|(id, event)| (id.clone(), Arc::clone(&event.serialized)))
                .collect(),
            completed_cell_order,
            next_cell_id: self.cell_id_limit,
        };
        let mut file = NamedTempFile::new_in(self.path.parent().ok_or("snapshot has no parent")?)
            .map_err(|error| error.to_string())?;
        super::write_buffered_json(file.as_file_mut(), &snapshot)?;
        if file
            .as_file()
            .metadata()
            .map_err(|error| error.to_string())?
            .len()
            > MAX_SNAPSHOT_BYTES as u64
        {
            return Err("named-state snapshot exceeds its byte limit".into());
        }
        file.as_file()
            .sync_all()
            .map_err(|error| error.to_string())?;
        Ok(StagedSnapshot {
            file,
            revision,
            completed_cells,
        })
    }

    /// The owned blocking transaction publishes after the atomic in-memory
    /// decision. Until this succeeds, callers must report uncertain durability.
    /// This is process-crash recovery, not guaranteed power-loss durability.
    pub(super) fn publish(&self, staged: StagedSnapshot) -> Result<(), String> {
        let StagedSnapshot {
            file,
            revision,
            completed_cells,
        } = staged;
        file.persist(&self.path)
            .map_err(|error| error.to_string())?;
        *self
            .completed_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = completed_cells;
        self.revision.store(revision, Ordering::Release);
        Ok(())
    }
}
