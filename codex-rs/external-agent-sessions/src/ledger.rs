use crate::now_unix_seconds;
use crate::records::stable_source_modified_at;
use codex_file_system::write_atomically;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

const SESSION_IMPORT_LEDGER_FILE: &str = "external_agent_session_imports.json";
const SESSION_IMPORT_LEDGER_LOCK_FILE: &str = "external_agent_session_imports.lock";
const SESSION_HASH_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ImportedExternalAgentSessionLedger {
    records: Vec<ImportedExternalAgentSessionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ImportedExternalAgentSessionRecord {
    source_path: PathBuf,
    content_sha256: String,
    imported_thread_id: ThreadId,
    imported_at: i64,
    #[serde(default)]
    source_modified_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedExternalAgentSessionImport {
    pub source_path: PathBuf,
    pub source_content_sha256: String,
    pub source_modified_at: Option<i64>,
    pub imported_thread_id: ThreadId,
}

#[derive(Debug)]
pub(super) struct ImportedSourceState<'a> {
    latest: &'a ImportedExternalAgentSessionRecord,
    hashes: HashSet<&'a str>,
}

impl ImportedSourceState<'_> {
    pub(super) fn current_source_refresh(&self) -> io::Result<Option<CurrentSourceRefresh>> {
        let (content_sha256, source_modified_at) = session_fingerprint(&self.latest.source_path)?;
        Ok(self
            .hashes
            .contains(content_sha256.as_str())
            .then(|| CurrentSourceRefresh {
                source_path: self.latest.source_path.clone(),
                content_sha256,
                source_modified_at,
                expected_latest_record: self.latest.clone(),
            }))
    }
}

#[derive(Debug)]
pub(super) struct CurrentSourceRefresh {
    source_path: PathBuf,
    content_sha256: String,
    source_modified_at: Option<i64>,
    expected_latest_record: ImportedExternalAgentSessionRecord,
}

pub fn has_current_session_been_imported(
    codex_home: &Path,
    source_path: &Path,
) -> io::Result<bool> {
    load_import_ledger(codex_home)?.contains_current_source(source_path)
}

#[cfg(test)]
pub(crate) fn record_imported_session(
    codex_home: &Path,
    source_path: &Path,
    imported_thread_id: ThreadId,
) -> io::Result<()> {
    let source_path = canonical_source_path(source_path)?;
    let (source_content_sha256, source_modified_at) = session_fingerprint(&source_path)?;
    record_completed_session_imports(
        codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_content_sha256,
            source_modified_at,
            source_path,
            imported_thread_id,
        }],
    )
}

/// Record the canonical path and hash captured by `PendingSessionImport`.
/// The source may no longer exist; completion never resolves or rereads it.
pub fn record_completed_session_imports(
    codex_home: &Path,
    imports: Vec<CompletedExternalAgentSessionImport>,
) -> io::Result<()> {
    if imports.is_empty() {
        return Ok(());
    }
    with_import_ledger_lock(codex_home, || {
        let mut ledger = load_import_ledger_unlocked(codex_home)?;
        let imported_at = now_unix_seconds();
        let mut updates = HashMap::new();
        for (order, import) in imports.into_iter().enumerate() {
            updates.insert(
                (
                    import.source_path.clone(),
                    import.source_content_sha256.clone(),
                ),
                (
                    order,
                    ImportedExternalAgentSessionRecord {
                        source_path: import.source_path,
                        content_sha256: import.source_content_sha256,
                        imported_thread_id: import.imported_thread_id,
                        imported_at,
                        source_modified_at: import.source_modified_at,
                    },
                ),
            );
        }
        ledger.records.retain(|record| {
            !updates.contains_key(&(record.source_path.clone(), record.content_sha256.clone()))
        });
        let mut updates = updates.into_values().collect::<Vec<_>>();
        updates.sort_unstable_by_key(|(order, _)| *order);
        ledger
            .records
            .extend(updates.into_iter().map(|(_, record)| record));
        save_import_ledger_unlocked(codex_home, &ledger)
    })
}

pub(super) fn record_current_source_refreshes(
    codex_home: &Path,
    refreshes: Vec<CurrentSourceRefresh>,
) -> io::Result<()> {
    if refreshes.is_empty() {
        return Ok(());
    }
    with_import_ledger_lock(codex_home, || {
        let mut ledger = load_import_ledger_unlocked(codex_home)?;
        let imported_at = now_unix_seconds();
        let mut latest = HashMap::new();
        let mut identities = HashMap::new();
        for (index, record) in ledger.records.iter().enumerate() {
            latest.insert(record.source_path.clone(), index);
            identities.insert(
                (record.source_path.clone(), record.content_sha256.clone()),
                index,
            );
        }
        let mut records = ledger.records.into_iter().map(Some).collect::<Vec<_>>();
        let mut changed = false;
        for refresh in refreshes {
            let Some(&latest_index) = latest.get(&refresh.source_path) else {
                continue;
            };
            if records[latest_index].as_ref() != Some(&refresh.expected_latest_record) {
                continue;
            }
            let key = (refresh.source_path.clone(), refresh.content_sha256);
            let Some(&index) = identities.get(&key) else {
                continue;
            };
            if index == latest_index
                && records[index]
                    .as_ref()
                    .is_some_and(|record| record.source_modified_at == refresh.source_modified_at)
            {
                continue;
            }
            let Some(mut record) = records[index].take() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "indexed ledger record is missing",
                ));
            };
            record.imported_at = imported_at;
            record.source_modified_at = refresh.source_modified_at;
            latest.insert(refresh.source_path, records.len());
            identities.insert(key, records.len());
            records.push(Some(record));
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        ledger.records = records.into_iter().flatten().collect();
        save_import_ledger_unlocked(codex_home, &ledger)
    })
}

impl ImportedExternalAgentSessionLedger {
    pub(super) fn source_states(&self) -> HashMap<&Path, ImportedSourceState<'_>> {
        let mut states = HashMap::new();
        for record in &self.records {
            let state = states
                .entry(record.source_path.as_path())
                .or_insert_with(|| ImportedSourceState {
                    latest: record,
                    hashes: HashSet::new(),
                });
            state.latest = record;
            state.hashes.insert(record.content_sha256.as_str());
        }
        states
    }

    pub(super) fn contains_fingerprint(&self, source_path: &Path, content_sha256: &str) -> bool {
        self.records.iter().any(|record| {
            record.source_path == source_path && record.content_sha256 == content_sha256
        })
    }

    pub(super) fn contains_current_source(&self, source_path: &Path) -> io::Result<bool> {
        if self.records.is_empty() {
            return Ok(false);
        }
        let source_path = canonical_source_path(source_path)?;
        if !self
            .records
            .iter()
            .any(|record| record.source_path == source_path)
        {
            return Ok(false);
        }
        let (content_sha256, _source_modified_at) = session_fingerprint(&source_path)?;
        Ok(self.records.iter().any(|record| {
            record.source_path == source_path && record.content_sha256 == content_sha256
        }))
    }

    #[cfg(test)]
    pub(super) fn current_source_refresh(
        &self,
        source_path: &Path,
    ) -> io::Result<Option<CurrentSourceRefresh>> {
        let source_path = canonical_source_path(source_path)?;
        self.source_states()
            .get(source_path.as_path())
            .map_or(Ok(None), ImportedSourceState::current_source_refresh)
    }
}

pub(super) fn load_import_ledger(
    codex_home: &Path,
) -> io::Result<ImportedExternalAgentSessionLedger> {
    with_import_ledger_lock(codex_home, || load_import_ledger_unlocked(codex_home))
}

fn load_import_ledger_unlocked(
    codex_home: &Path,
) -> io::Result<ImportedExternalAgentSessionLedger> {
    let path = import_ledger_path(codex_home);
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(ImportedExternalAgentSessionLedger::default());
        }
        Err(err) => return Err(err),
    };
    serde_json::from_str(&raw).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid external agent session import ledger: {err}"),
        )
    })
}

fn save_import_ledger_unlocked(
    codex_home: &Path,
    ledger: &ImportedExternalAgentSessionLedger,
) -> io::Result<()> {
    let path = import_ledger_path(codex_home);
    let raw = serde_json::to_string_pretty(ledger).map_err(io::Error::other)?;
    write_atomically(&path, &raw)
}

fn with_import_ledger_lock<T>(
    codex_home: &Path,
    operation: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    fs::create_dir_all(codex_home)?;
    let lock_path = codex_home.join(SESSION_IMPORT_LEDGER_LOCK_FILE);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock_file.lock()?;
    operation()
}

fn import_ledger_path(codex_home: &Path) -> PathBuf {
    codex_home.join(SESSION_IMPORT_LEDGER_FILE)
}

fn canonical_source_path(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path)
}

fn session_fingerprint(path: &Path) -> io::Result<(String, Option<i64>)> {
    let mut file = File::open(path)?;
    let metadata_before = file.metadata().ok();
    let mut hasher = Sha256::new();
    let mut buffer = [0; SESSION_HASH_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let metadata_after = file.metadata().ok();
    let source_modified_at = metadata_before
        .as_ref()
        .zip(metadata_after.as_ref())
        .and_then(|(before, after)| stable_source_modified_at(before, after));
    Ok((format!("{digest:x}"), source_modified_at))
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
