use crate::now_unix_seconds;
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedExternalAgentSessionImport {
    pub source_path: PathBuf,
    pub source_content_sha256: String,
    pub imported_thread_id: ThreadId,
}

#[cfg(test)]
pub(crate) fn record_imported_session(
    codex_home: &Path,
    source_path: &Path,
    imported_thread_id: ThreadId,
) -> io::Result<()> {
    let source_path = fs::canonicalize(source_path)?;
    let source_content_sha256 = session_content_sha256(&source_path)?;
    record_completed_session_imports(
        codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_content_sha256,
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

impl ImportedExternalAgentSessionLedger {
    /// Returns every imported content hash, keyed by canonical source path.
    pub(super) fn imported_hashes_by_source(&self) -> HashMap<&Path, HashSet<&str>> {
        let mut hashes = HashMap::<_, HashSet<_>>::new();
        for record in &self.records {
            hashes
                .entry(record.source_path.as_path())
                .or_default()
                .insert(record.content_sha256.as_str());
        }
        hashes
    }

    pub(super) fn contains_fingerprint(&self, source_path: &Path, content_sha256: &str) -> bool {
        self.records.iter().any(|record| {
            record.source_path == source_path && record.content_sha256 == content_sha256
        })
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

pub(super) fn session_content_sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; SESSION_HASH_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
