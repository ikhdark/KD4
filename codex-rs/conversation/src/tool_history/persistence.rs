//! Completed-tool history disk compatibility, replay, load/write admission and durability.

use std::collections::BTreeMap;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde_json::value::RawValue;

use super::ArtifactOrigin;
use super::ToolHistoryMutation;
use super::ToolHistoryState;
use super::sha256;

const JOURNAL_VERSION: u8 = 1;
const LEDGER_VERSION: u8 = 1;

#[derive(Deserialize, Serialize)]
struct ToolHistoryLedgerFile {
    version: u8,
    #[serde(default)]
    journal_sequences: BTreeMap<String, u64>,
    state: ToolHistoryState,
}

#[derive(Serialize)]
struct ToolHistoryLedgerRef<'a> {
    version: u8,
    journal_sequences: &'a BTreeMap<String, u64>,
    state: &'a ToolHistoryState,
}

#[derive(Deserialize)]
struct ToolHistoryJournalRecord {
    version: u8,
    writer_id: String,
    sequence: u64,
    mutation: ToolHistoryMutation,
    checksum_sha256: String,
}

#[derive(Serialize)]
struct ToolHistoryJournalRecordRef<'a> {
    version: u8,
    writer_id: &'a str,
    sequence: u64,
    mutation: &'a ToolHistoryMutation,
    checksum_sha256: String,
}

#[derive(Serialize)]
struct ToolHistoryJournalChecksumRef<'a, M: ?Sized> {
    version: u8,
    writer_id: &'a str,
    sequence: u64,
    mutation: &'a M,
}

#[derive(Deserialize)]
struct ToolHistoryJournalMutationRef<'a> {
    #[serde(borrow)]
    mutation: &'a RawValue,
}

#[derive(Debug)]
enum ToolHistoryJournalLoadError {
    Corrupt(String),
    UnsupportedVersion(u64),
    Io(String),
}

trait ToolHistoryVersion {
    fn history_version(&self) -> u64;
}

impl ToolHistoryVersion for ToolHistoryLedgerFile {
    fn history_version(&self) -> u64 { u64::from(self.version) }
}

impl ToolHistoryVersion for ToolHistoryJournalRecord {
    fn history_version(&self) -> u64 { u64::from(self.version) }
}

#[derive(Deserialize)]
struct CheckpointSequences {
    version: u64,
    #[serde(default)]
    journal_sequences: BTreeMap<String, u64>,
}

impl ToolHistoryVersion for CheckpointSequences {
    fn history_version(&self) -> u64 { self.version }
}

#[derive(Deserialize)]
struct ToolHistoryVersionHeader {
    version: u64,
}

impl ToolHistoryVersion for ToolHistoryVersionHeader {
    fn history_version(&self) -> u64 { self.version }
}

fn decode_tool_history_version<T: serde::de::DeserializeOwned + ToolHistoryVersion>(
    bytes: &[u8],
    supported: u8,
) -> Result<T, ToolHistoryJournalLoadError> {
    match serde_json::from_slice::<T>(bytes) {
        Ok(value) if value.history_version() == u64::from(supported) => Ok(value),
        Ok(value) => Err(ToolHistoryJournalLoadError::UnsupportedVersion(value.history_version())),
        Err(error) => {
            // Future schemas and versions outside u8 still need the explicit
            // unsupported outcome. Only failed typed decodes traverse twice.
            if let Ok(header) = serde_json::from_slice::<ToolHistoryVersionHeader>(bytes)
                && header.version != u64::from(supported)
            {
                return Err(ToolHistoryJournalLoadError::UnsupportedVersion(header.version));
            }
            Err(ToolHistoryJournalLoadError::Corrupt(error.to_string()))
        }
    }
}

#[derive(Debug)]
pub enum ToolHistoryLoadOutcome {
    Missing,
    Loaded(ToolHistoryState),
    /// The checkpoint plus every journal record before the first invalid one.
    RecoveredJournalPrefix {
        state: ToolHistoryState,
        path: std::path::PathBuf,
        error: String,
        /// This load checkpointed the prefix and quarantined the journal, so a
        /// later load does not report it again.
        source_retired: bool,
    },
    Corrupt {
        path: std::path::PathBuf,
        error: String,
        /// This load quarantined the ledger, so a later load does not report it again.
        source_retired: bool,
    },
    UnsupportedVersion {
        path: std::path::PathBuf,
        found: u64,
        supported: u8,
    },
    IoFailure {
        path: std::path::PathBuf,
        error: String,
    },
}

impl ToolHistoryLoadOutcome {
    /// True when the warning reports a change this load made to the thread's
    /// durable ledger state. Unsupported, unreadable or unretired sources are
    /// reported again by every load.
    pub fn warning_is_one_shot(&self) -> bool {
        matches!(
            self,
            Self::RecoveredJournalPrefix { source_retired: true, .. }
                | Self::Corrupt { source_retired: true, .. }
        )
    }

    pub fn into_state_and_warning(self) -> (ToolHistoryState, Option<String>) {
        match self {
            Self::Missing => (ToolHistoryState::default(), None),
            Self::Loaded(state) => (state, None),
            Self::RecoveredJournalPrefix { state, path, error, .. } => (
                state,
                Some(format!(
                    "Recovered completed-tool history up to the first corrupt record of journal {}: {error}",
                    path.display()
                )),
            ),
            Self::Corrupt { path, error, .. } => (
                ToolHistoryState::default(),
                Some(format!(
                    "Ignoring corrupt completed-tool history ledger {}: {error}",
                    path.display()
                )),
            ),
            Self::UnsupportedVersion {
                path,
                found,
                supported,
            } => (
                ToolHistoryState::default(),
                Some(format!(
                    "Completed-tool history ledger {} has unsupported version {found}; this build supports version {supported}. Tool-history writes are disabled while the unsupported file is present",
                    path.display()
                )),
            ),
            Self::IoFailure { path, error } => (
                ToolHistoryState::default(),
                Some(format!(
                    "Could not read completed-tool history ledger {}: {error}",
                    path.display()
                )),
            ),
        }
    }
}

/// Initialization owns validation after history reconstruction, immediately
/// before its durable replacement. Do not hash the same set during loading.
pub async fn load_tool_history_state_for_initialization(
    codex_home: &std::path::Path,
    thread_id: &str,
) -> ToolHistoryLoadOutcome {
    match load_tool_history_state_for_fork(codex_home, thread_id).await {
        ToolHistoryLoadOutcome::RecoveredJournalPrefix { state, path, error, .. } => {
            // The source must survive until a durable checkpoint owns its valid
            // prefix. A failed or cancelled recovery can then safely be retried.
            let (error, source_retired) = match persist_tool_history_checkpoint(
                codex_home,
                thread_id,
                &state,
                Arc::default(),
                true,
            )
            .await
            {
                Ok(()) => (
                    format!("{error}; recovered prefix checkpointed and journal quarantined"),
                    true,
                ),
                Err(commit_error) => (
                    format!("{error}; failed to checkpoint recovered prefix: {commit_error}"),
                    false,
                ),
            };
            ToolHistoryLoadOutcome::RecoveredJournalPrefix {
                state,
                path,
                error,
                source_retired,
            }
        }
        ToolHistoryLoadOutcome::Corrupt { path, error, .. } => {
            let quarantine_path = corrupt_ledger_quarantine_path(&path);
            match tokio::fs::rename(&path, &quarantine_path).await {
                Ok(()) => ToolHistoryLoadOutcome::Corrupt {
                    path: quarantine_path.clone(),
                    error: format!(
                        "{error}; quarantined from {} to {}",
                        path.display(),
                        quarantine_path.display()
                    ),
                    source_retired: true,
                },
                Err(rename_error) => ToolHistoryLoadOutcome::Corrupt {
                    path,
                    error: format!("{error}; failed to quarantine ledger: {rename_error}"),
                    source_retired: false,
                },
            }
        }
        outcome => outcome,
    }
}

fn corrupt_ledger_quarantine_path(path: &std::path::Path) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_extension(format!("corrupt-{}-{nonce}", std::process::id()))
}

/// Reads a parent ledger for fork without reconciling the parent's protection markers.
///
/// The parent can still be live while the child is initialized. Mutating its artifact ownership
/// from the child would race with a parent tool result between marker creation and ledger persist.
pub async fn load_tool_history_state_for_fork(
    codex_home: &std::path::Path,
    thread_id: &str,
) -> ToolHistoryLoadOutcome {
    let path = ledger_path(codex_home, thread_id);
    let lock_path = path.clone();
    let transaction = tokio::task::spawn_blocking(move || {
        codex_file_system::acquire_atomic_write_lock(&lock_path)
    })
    .await
    .map_err(|error| error.to_string())
    .and_then(|result| result.map_err(|error| error.to_string()));
    let _transaction = match transaction {
        Ok(owner) => owner,
        Err(error) => return ToolHistoryLoadOutcome::IoFailure { path, error },
    };
    let (mut state, checkpoint_exists, mut journal_sequences) = match tokio::fs::read(&path).await {
        Ok(bytes) => {
            match decode_tool_history_version::<ToolHistoryLedgerFile>(&bytes, LEDGER_VERSION) {
                Ok(mut file) => {
                    // Ordered recovery mutations need origins, not expensive
                    // receipt derivation. Derive final candidates after replay.
                    file.state.rebuild_artifact_index();
                    (file.state, true, file.journal_sequences)
                }
                Err(ToolHistoryJournalLoadError::UnsupportedVersion(found)) => {
                    return ToolHistoryLoadOutcome::UnsupportedVersion {
                        path,
                        found,
                        supported: LEDGER_VERSION,
                    };
                }
                Err(error) => {
                    return ToolHistoryLoadOutcome::Corrupt {
                        path,
                        error: format!("{error:?}"),
                        source_retired: false,
                    };
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (ToolHistoryState::default(), false, BTreeMap::new())
        }
        Err(error) => {
            return ToolHistoryLoadOutcome::IoFailure {
                path,
                error: error.to_string(),
            };
        }
    };
    let journal_path = journal_path(codex_home, thread_id);
    let journal_exists =
        match replay_tool_history_journal(&journal_path, &mut state, &mut journal_sequences, true)
            .await
        {
            Ok(exists) => exists,
            Err(ToolHistoryJournalLoadError::Corrupt(error)) => {
                // Replay applied each checksummed record before the invalid
                // one, a state that was durably reached, as with a torn tail.
                state.refresh_derived_and_indexes();
                hydrate_legacy_artifact_directories(codex_home, thread_id, &mut state).await;
                return ToolHistoryLoadOutcome::RecoveredJournalPrefix {
                    state,
                    path: journal_path,
                    error,
                    source_retired: false,
                };
            }
            Err(ToolHistoryJournalLoadError::UnsupportedVersion(found)) => {
                return ToolHistoryLoadOutcome::UnsupportedVersion {
                    path: journal_path,
                    found,
                    supported: JOURNAL_VERSION,
                };
            }
            Err(ToolHistoryJournalLoadError::Io(error)) => {
                return ToolHistoryLoadOutcome::IoFailure {
                    path: journal_path,
                    error,
                };
            }
        };
    if !checkpoint_exists && !journal_exists {
        ToolHistoryLoadOutcome::Missing
    } else {
        state.refresh_derived_and_indexes();
        hydrate_legacy_artifact_directories(codex_home, thread_id, &mut state).await;
        ToolHistoryLoadOutcome::Loaded(state)
    }
}

async fn hydrate_legacy_artifact_directories(
    home: &Path,
    thread: &str,
    state: &mut ToolHistoryState,
) {
    let directories = state
        .internal_artifact_origins
        .iter()
        .filter(|(id, ArtifactOrigin(source, _, _, _))| {
            source == "context:artifact_directory"
                && !state.artifact_directory_members.contains_key(*id)
        })
        .map(|(id, ArtifactOrigin(_, bytes, sha, _))| (id.clone(), *bytes, sha.clone()))
        .collect::<Vec<_>>();
    for (id, expected_bytes, expected_sha) in directories {
        let Ok(bytes) = codex_tool_artifacts::read_complete_canonical_snapshot(
            home,
            thread,
            &id,
            usize::try_from(expected_bytes).unwrap_or(0),
        )
        .await
        else {
            continue;
        };
        if bytes.len() as u64 != expected_bytes || sha256(&bytes) != expected_sha {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let Some(items) = value["items"].as_array() else {
            continue;
        };
        state.artifact_directory_members.insert(
            id,
            items
                .iter()
                .filter_map(|pin| pin["artifact_id"].as_str().map(str::to_string))
                .collect(),
        );
    }
}

async fn replay_tool_history_journal(
    path: &std::path::Path,
    state: &mut ToolHistoryState,
    checkpoint_sequences: &mut BTreeMap<String, u64>,
    apply: bool,
) -> Result<bool, ToolHistoryJournalLoadError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(ToolHistoryJournalLoadError::Io(error.to_string())),
    };
    replay_tool_history_journal_bytes(path, &bytes, state, checkpoint_sequences, apply)?;
    Ok(true)
}

fn replay_tool_history_journal_bytes(
    path: &std::path::Path,
    bytes: &[u8],
    state: &mut ToolHistoryState,
    checkpoint_sequences: &mut BTreeMap<String, u64>,
    apply: bool,
) -> Result<(), ToolHistoryJournalLoadError> {
    let mut offset = 0_usize;
    let mut writer_sequences = BTreeMap::<String, u64>::new();
    while let Some(relative_end) = memchr::memchr(b'\n', &bytes[offset..]) {
        let end = offset + relative_end;
        let line = &bytes[offset..end];
        offset = end + 1;
        if line.is_empty() {
            continue;
        }
        let record =
            decode_tool_history_version::<ToolHistoryJournalRecord>(line, JOURNAL_VERSION)?;
        if record.version != JOURNAL_VERSION {
            return Err(ToolHistoryJournalLoadError::UnsupportedVersion(u64::from(
                record.version,
            )));
        }
        let checksum = tool_history_journal_checksum(
            record.writer_id.as_str(),
            record.sequence,
            &record.mutation,
        )
        .map_err(ToolHistoryJournalLoadError::Corrupt)?;
        if checksum != record.checksum_sha256 && !matches_legacy_scope_checksum(line, &record)
        {
            return Err(ToolHistoryJournalLoadError::Corrupt(format!(
                "journal checksum mismatch for writer {} sequence {}",
                record.writer_id, record.sequence
            )));
        }
        if let Some(previous) = writer_sequences.insert(record.writer_id.clone(), record.sequence)
            && record.sequence != previous.saturating_add(1)
        {
            return Err(ToolHistoryJournalLoadError::Corrupt(format!(
                "non-contiguous journal sequence for writer {}: {previous} then {}",
                record.writer_id, record.sequence
            )));
        }
        let checkpoint = checkpoint_sequences.entry(record.writer_id).or_default();
        if record.sequence > *checkpoint {
            if record.sequence != checkpoint.saturating_add(1) {
                return Err(ToolHistoryJournalLoadError::Corrupt(format!(
                    "journal gap after checkpoint {}: first uncheckpointed sequence {}",
                    *checkpoint, record.sequence
                )));
            }
            if apply {
                if let ToolHistoryMutation::RegisterCandidate { candidate } = &record.mutation {
                    state.register_candidate((**candidate).clone(), false);
                } else {
                    record.mutation.apply(state);
                }
            }
            *checkpoint = record.sequence;
        }
    }
    if offset != bytes.len() {
        tracing::warn!(target: "codex_core::tool_history",
            path = %path.display(),
            trailing_bytes = bytes.len() - offset,
            "ignoring incomplete trailing completed-tool history journal record"
        );
    }
    Ok(())
}

fn matches_legacy_scope_checksum(line: &[u8], record: &ToolHistoryJournalRecord) -> bool {
    // Older v1 workspace evidence recorded success but had no scope version.
    // Check what the writer hashed without widening compatibility to records
    // that lack an outcome or to other, unrecognized mutation shapes.
    if !matches!(record.mutation, ToolHistoryMutation::RegisterWorkspaceEvidence { .. }) {
        return false;
    }
    let Ok(original) = serde_json::from_slice::<ToolHistoryJournalMutationRef<'_>>(line) else {
        return false;
    };
    #[derive(Deserialize)]
    struct ObservationFields {
        successful: Option<bool>,
        source_scope_version: Option<u8>,
    }
    #[derive(Deserialize)]
    struct WorkspaceMutation {
        observation: ObservationFields,
    }
    let Ok(legacy) = serde_json::from_str::<WorkspaceMutation>(original.mutation.get()) else {
        return false;
    };
    legacy.observation.successful.is_some()
        && legacy.observation.source_scope_version.is_none()
        && tool_history_journal_checksum(&record.writer_id, record.sequence, original.mutation)
            .is_ok_and(|checksum| checksum == record.checksum_sha256)
}

fn tool_history_journal_checksum(
    writer_id: &str,
    sequence: u64,
    mutation: &(impl Serialize + ?Sized),
) -> Result<String, String> {
    let bytes = serde_json::to_vec(&ToolHistoryJournalChecksumRef {
        version: JOURNAL_VERSION,
        writer_id,
        sequence,
        mutation,
    })
    .map_err(|error| format!("failed to serialize tool-history journal checksum: {error}"))?;
    Ok(sha256(&bytes))
}

#[derive(Clone, Eq, PartialEq)]
struct JournalStamp {
    len: u64,
    modified: std::time::SystemTime,
    created: Option<std::time::SystemTime>,
}

fn journal_stamp(path: &Path) -> std::io::Result<Option<JournalStamp>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(JournalStamp {
            len: metadata.len(),
            modified: metadata.modified()?,
            created: metadata.created().ok(),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Owned by the persistence worker, under the existing per-thread I/O permit.
/// Unknown startup state or any observed external write invalidates the replay
/// boundary; only a successful checkpoint establishes a trusted boundary.
#[derive(Default)]
pub struct ToolHistoryJournalWriter {
    file: Option<std::fs::File>,
    journal_stamp: Option<JournalStamp>,
    ledger_stamp: Option<JournalStamp>,
    sequences: Option<BTreeMap<String, u64>>,
}

pub async fn persist_tool_history_state(
    codex_home: &std::path::Path,
    thread_id: &str,
    state: &ToolHistoryState,
) -> Result<(), String> {
    persist_tool_history_state_with_writer(codex_home, thread_id, state, Arc::default()).await
}

pub async fn persist_tool_history_state_with_writer(
    codex_home: &std::path::Path,
    thread_id: &str,
    state: &ToolHistoryState,
    writer: Arc<std::sync::Mutex<ToolHistoryJournalWriter>>,
) -> Result<(), String> {
    persist_tool_history_checkpoint(codex_home, thread_id, state, writer, false).await
}

async fn persist_tool_history_checkpoint(
    codex_home: &std::path::Path,
    thread_id: &str,
    state: &ToolHistoryState,
    writer: Arc<std::sync::Mutex<ToolHistoryJournalWriter>>,
    quarantine_corrupt_journal: bool,
) -> Result<(), String> {
    codex_tool_artifacts::sync_tool_output_artifacts(codex_home, thread_id)
        .await
        .map_err(|error| format!("failed to sync checkpoint artifacts: {error}"))?;
    let path = ledger_path(codex_home, thread_id);
    let journal_path = journal_path(codex_home, thread_id);
    let lock_path = path.clone();
    let transaction_owner = tokio::task::spawn_blocking(move || {
        // The transaction lock lives in the ledger directory; report its
        // creation failure as the ledger-directory failure it is.
        if let Some(directory) = lock_path.parent() {
            std::fs::create_dir_all(directory)
                .map_err(|err| format!("failed to create tool-history ledger directory: {err}"))?;
        }
        codex_file_system::acquire_atomic_write_lock(&lock_path).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())??;
    if state.is_persisted_empty()
        && tool_history_storage_is_definitely_absent(&path, &journal_path).await
    {
        return Ok(());
    }
    // Production callers hold the per-thread I/O permit: this snapshot
    // supersedes every record currently in the journal, including old writers.
    let cached_boundary = {
        let writer = Arc::clone(&writer);
        let path = path.clone();
        let journal_path = journal_path.clone();
        tokio::task::spawn_blocking(move || {
            let writer = writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (journal_stamp(&path).ok().as_ref() == Some(&writer.ledger_stamp)
                && journal_stamp(&journal_path).ok().as_ref() == Some(&writer.journal_stamp))
            .then(|| writer.sequences.clone())
            .flatten()
        })
        .await
        .map_err(|error| error.to_string())?
    };
    let journal_sequences = if let Some(sequences) = cached_boundary {
        sequences
    } else {
        let mut journal_sequences = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                decode_tool_history_version::<CheckpointSequences>(&bytes, LEDGER_VERSION)
                    .map_err(|error| {
                        format!("failed to read checkpoint journal boundary: {error:?}")
                    })?
                    .journal_sequences
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                return Err(format!(
                    "failed to read checkpoint journal boundary: {error}"
                ));
            }
        };
        match replay_tool_history_journal(
            &journal_path,
            &mut ToolHistoryState::default(),
            &mut journal_sequences,
            false,
        )
        .await
        {
            Ok(_) => {}
            Err(ToolHistoryJournalLoadError::Corrupt(_)) if quarantine_corrupt_journal => {}
            Err(error) => {
                return Err(format!(
                    "failed to validate checkpoint journal boundary: {error:?}"
                ));
            }
        }
        journal_sequences
    };
    let bytes = serde_json::to_vec(&ToolHistoryLedgerRef {
        version: LEDGER_VERSION,
        journal_sequences: &journal_sequences,
        state,
    })
    .map_err(|err| format!("failed to serialize tool-history ledger: {err}"))?;
    tokio::task::spawn_blocking(move || {
        let _transaction_owner = transaction_owner;
        let mut writer = writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writer.sequences = None;
        // Close the retained handle before checkpoint compaction removes it.
        writer.file = None;
        let directory = path
            .parent()
            .ok_or_else(|| "tool-history ledger has no parent directory".to_string())?;
        std::fs::create_dir_all(directory)
            .map_err(|err| format!("failed to create tool-history ledger directory: {err}"))?;
        let mut temp = tempfile::NamedTempFile::new_in(directory)
            .map_err(|err| format!("failed to create tool-history ledger temporary: {err}"))?;
        temp.write_all(&bytes)
            .map_err(|err| format!("failed to write tool-history ledger: {err}"))?;
        temp.as_file_mut()
            .sync_all()
            .map_err(|err| format!("failed to sync tool-history ledger: {err}"))?;
        codex_file_system::persist_synced_file(temp, &path, directory)
            .map_err(|err| format!("failed to commit tool-history ledger: {err}"))?;
        // Commit first: a crash before retiring the source replays only records
        // beyond this checkpoint's sequence boundary, never loses its prefix.
        let retire = if quarantine_corrupt_journal {
            std::fs::rename(&journal_path, corrupt_ledger_quarantine_path(&journal_path))
        } else {
            std::fs::remove_file(&journal_path)
        };
        match retire {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to clear compacted tool-history journal: {error}"
                ));
            }
        }
        sync_tool_history_ledger_directory(directory)?;
        writer.ledger_stamp = journal_stamp(&path).map_err(|error| error.to_string())?;
        writer.journal_stamp = None;
        writer.sequences = Some(journal_sequences);
        Ok(())
    })
    .await
    .map_err(|err| format!("tool-history ledger writer failed: {err}"))?
}

async fn tool_history_storage_is_definitely_absent(
    ledger_path: &std::path::Path,
    journal_path: &std::path::Path,
) -> bool {
    for path in [ledger_path, journal_path] {
        match tokio::fs::try_exists(path).await {
            Ok(false) => {}
            Ok(true) | Err(_) => return false,
        }
    }
    true
}

pub async fn persist_tool_history_mutations_with_writer(
    codex_home: &std::path::Path,
    thread_id: &str,
    writer_id: &str,
    mutations: &[(u64, ToolHistoryMutation)],
    writer: Arc<std::sync::Mutex<ToolHistoryJournalWriter>>,
) -> Result<u64, String> {
    if mutations.is_empty() {
        return Ok(0);
    }
    let path = journal_path(codex_home, thread_id);
    let mut bytes = Vec::new();
    for (sequence, mutation) in mutations {
        let checksum_sha256 = tool_history_journal_checksum(writer_id, *sequence, mutation)?;
        serde_json::to_writer(
            &mut bytes,
            &ToolHistoryJournalRecordRef {
                version: JOURNAL_VERSION,
                writer_id,
                sequence: *sequence,
                mutation,
                checksum_sha256,
            },
        )
        .map_err(|error| format!("failed to serialize tool-history journal: {error}"))?;
        bytes.push(b'\n');
    }
    let persisted_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let writer_id = writer_id.to_string();
    let final_sequence = mutations
        .last()
        .map(|(sequence, _)| *sequence)
        .unwrap_or_default();
    let ledger = ledger_path(codex_home, thread_id);
    tokio::task::spawn_blocking(move || {
        let _transaction = codex_file_system::acquire_atomic_write_lock(&ledger)
            .map_err(|error| error.to_string())?;
        let mut writer = writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_stamp = journal_stamp(&path).map_err(|error| error.to_string())?;
        let unchanged = current_stamp == writer.journal_stamp;
        if !unchanged || journal_stamp(&ledger).ok().as_ref() != Some(&writer.ledger_stamp) {
            writer.sequences = None;
            writer.file = None;
        }
        if writer.file.is_none() {
            // Validate an unknown or externally changed source before opening
            // an append handle (including before repairing an incomplete tail).
            // A journal may begin after records already compacted into the ledger.
            let mut checkpoint_sequences = match std::fs::read(&ledger) {
                Ok(bytes) => {
                    decode_tool_history_version::<CheckpointSequences>(&bytes, LEDGER_VERSION)
                        .map_err(|error| format!("tool-history writes disabled: {error:?}"))?
                        .journal_sequences
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
                Err(error) => return Err(format!("failed to read tool-history ledger: {error}")),
            };
            match std::fs::read(&path) {
                Ok(bytes) => replay_tool_history_journal_bytes(
                    &path,
                    &bytes,
                    &mut ToolHistoryState::default(),
                    &mut checkpoint_sequences,
                    false,
                )
                .map_err(|error| format!("tool-history writes disabled: {error:?}"))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("failed to read tool-history journal: {error}")),
            }
        }
        let directory = path
            .parent()
            .ok_or_else(|| "tool-history journal has no parent directory".to_string())?;
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("failed to create tool-history journal directory: {error}"))?;
        let retained = writer.file.is_some();
        let mut file = match writer.file.take() {
            Some(file) => file,
            None => std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .append(true)
                .truncate(false)
                .open(&path)
                .map_err(|error| format!("failed to open tool-history journal: {error}"))?,
        };
        let existing_len = current_stamp.as_ref().map_or(0, |stamp| stamp.len);
        if existing_len > 0 && !retained {
            // A complete journal needs only its final byte inspected. Scan an
            // incomplete tail backwards with fixed memory instead of rereading
            // the full journal into a growing allocation on every append.
            let mut chunk = [0_u8; 8 * 1024];
            let mut remaining = existing_len;
            let mut scan_bytes = 1;
            let complete_len = loop {
                if remaining == 0 {
                    break 0;
                }
                let chunk_start = remaining.saturating_sub(scan_bytes);
                let chunk_len = (remaining - chunk_start) as usize;
                file.seek(SeekFrom::Start(chunk_start))
                    .map_err(|error| format!("failed to seek tool-history journal: {error}"))?;
                file.read_exact(&mut chunk[..chunk_len])
                    .map_err(|error| format!("failed to read tool-history journal: {error}"))?;
                if let Some(index) = chunk[..chunk_len].iter().rposition(|byte| *byte == b'\n') {
                    break chunk_start + index as u64 + 1;
                }
                remaining = chunk_start;
                scan_bytes = chunk.len() as u64;
            };
            if complete_len < existing_len {
                // An append-only Windows handle cannot truncate. Keep append
                // semantics for records and use a writable handle for repair.
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .and_then(|repair| repair.set_len(complete_len))
                    .map_err(|error| {
                        format!("failed to repair incomplete tool-history journal: {error}")
                    })?;
            }
        }
        let append_offset = file
            .seek(SeekFrom::End(0))
            .map_err(|error| format!("failed to seek tool-history journal append: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("failed to append tool-history journal: {error}"))?;
        writer.journal_stamp = journal_stamp(&path).map_err(|error| error.to_string())?;
        if (retained && append_offset != existing_len)
            || writer
                .journal_stamp
                .as_ref()
                .is_none_or(|stamp| stamp.len != append_offset.saturating_add(persisted_bytes))
        {
            writer.sequences = None;
        }
        writer.file = Some(file);
        if let Some(sequences) = &mut writer.sequences {
            sequences.insert(writer_id, final_sequence);
        }
        // Appends must be visible to readers, but crash durability belongs to
        // the ordered terminal checkpoint, alongside the rollout flush.
        Ok(persisted_bytes)
    })
    .await
    .map_err(|error| format!("tool-history journal writer failed: {error}"))?
}

fn ledger_path(codex_home: &std::path::Path, thread_id: &str) -> std::path::PathBuf {
    codex_home
        .join("tool-history")
        .join(format!("{thread_id}.json"))
}

fn journal_path(codex_home: &std::path::Path, thread_id: &str) -> std::path::PathBuf {
    codex_home
        .join("tool-history")
        .join(format!("{thread_id}.journal.jsonl"))
}

fn sync_tool_history_ledger_directory(directory: &std::path::Path) -> Result<(), String> {
    sync_tool_history_ledger_directory_impl(directory)
}

fn sync_tool_history_ledger_directory_impl(directory: &std::path::Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("failed to sync tool-history directory: {error}"))?;
    #[cfg(not(unix))]
    let _ = directory; // The Windows checkpoint rename uses write-through.
    Ok(())
}
