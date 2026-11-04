use super::now_unix_seconds;
use codex_file_system::write_bytes_atomically_preserving_permissions;
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    reservations: Vec<ImportedExternalAgentSessionReservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ImportedExternalAgentSessionReservation {
    source_path: PathBuf,
    content_sha256: String,
    imported_thread_id: ThreadId,
}

/// The file descriptor owns the cross-process lock until persistence and ledger
/// publication have both settled. A process crash releases it, not its reservation.
pub(super) struct SessionImportLedgerGuard {
    _lock_file: File,
    codex_home: PathBuf,
    ledger: ImportedExternalAgentSessionLedger,
}

impl SessionImportLedgerGuard {
    pub(super) fn contains_fingerprint(&self, source_path: &Path, content_sha256: &str) -> bool {
        self.ledger.contains_fingerprint(source_path, content_sha256)
    }

    pub(super) fn contains_source(&self, source_path: &Path) -> bool {
        self.ledger.records.iter().any(|record| record.source_path == source_path)
    }

    /// Existing reservations require inspection; a freshly allocated identity
    /// cannot yet have durable import data and needs no full-store absence scan.
    pub(super) fn reserve(&mut self, source_path: &Path, content_sha256: &str) -> io::Result<(ThreadId, bool)> {
        if let Some(reservation) = self.ledger.reservations.iter().find(|reservation| {
            reservation.source_path == source_path && reservation.content_sha256 == content_sha256
        }) {
            return Ok((reservation.imported_thread_id, true));
        }
        let thread_id = ThreadId::new();
        self.ledger.reservations.push(ImportedExternalAgentSessionReservation {
            source_path: source_path.to_path_buf(),
            content_sha256: content_sha256.to_string(),
            imported_thread_id: thread_id,
        });
        // Persist the identity before ThreadStore can create any durable data.
        save_import_ledger_unlocked(&self.codex_home, &self.ledger)?;
        Ok((thread_id, false))
    }

    pub(super) fn complete(
        &mut self,
        source_path: PathBuf,
        content_sha256: String,
        imported_thread_id: ThreadId,
    ) -> io::Result<()> {
        if let Some(record) = self.ledger.records.iter().find(|record| {
            record.source_path == source_path && record.content_sha256 == content_sha256
        }) {
            if record.imported_thread_id != imported_thread_id {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "session fingerprint already belongs to a different imported thread"));
            }
            return Ok(());
        }
        if !self.ledger.reservations.iter().any(|reservation| {
            reservation.source_path == source_path
                && reservation.content_sha256 == content_sha256
                && reservation.imported_thread_id == imported_thread_id
        }) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "session completion does not own its durable reservation"));
        }
        self.ledger.records.push(ImportedExternalAgentSessionRecord {
            source_path,
            content_sha256,
            imported_thread_id,
            imported_at: now_unix_seconds(),
        });
        // Keep the reservation until the same atomic replacement publishes completion.
        self.ledger.reservations.retain(|reservation| reservation.imported_thread_id != imported_thread_id);
        save_import_ledger_unlocked(&self.codex_home, &self.ledger)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ImportedExternalAgentSessionRecord {
    source_path: PathBuf,
    content_sha256: String,
    imported_thread_id: ThreadId,
    imported_at: i64,
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
    with_import_ledger_lock(codex_home, None, || load_import_ledger_unlocked(codex_home))
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
    let paths = codex_file_system::resolve_symlink_write_paths(&path)?;
    write_bytes_atomically_preserving_permissions(&paths.write_path, raw.as_bytes())
}

fn with_import_ledger_lock<T>(
    codex_home: &Path,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
    operation: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let _lock = lock_import_ledger_file(codex_home, cancellation)?;
    operation()
}

pub(super) fn lock_import_ledger(
    codex_home: &Path,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> io::Result<SessionImportLedgerGuard> {
    let lock_file = lock_import_ledger_file(codex_home, cancellation)?;
    let ledger = load_import_ledger_unlocked(codex_home)?;
    Ok(SessionImportLedgerGuard {
        _lock_file: lock_file,
        codex_home: codex_home.to_path_buf(),
        ledger,
    })
}

fn lock_import_ledger_file(
    codex_home: &Path,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> io::Result<File> {
    fs::create_dir_all(codex_home)?;
    let lock_path = codex_home.join(SESSION_IMPORT_LEDGER_LOCK_FILE);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    loop {
        if let Some(cancellation) = cancellation { super::super::files::check_cancel(cancellation)?; }
        match fs2::FileExt::try_lock_exclusive(&lock_file) {
            Ok(()) => return Ok(lock_file),
            Err(err) if err.raw_os_error() == fs2::lock_contended_error().raw_os_error() => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(err) => return Err(err),
        }
    }
}

fn import_ledger_path(codex_home: &Path) -> PathBuf {
    codex_home.join(SESSION_IMPORT_LEDGER_FILE)
}

pub(super) fn session_content_sha256(path: &Path) -> io::Result<String> {
    session_content_sha256_with_cancellation(path, None)
}

pub(super) fn session_content_sha256_with_cancellation(
    path: &Path,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; SESSION_HASH_BUFFER_SIZE];
    loop {
        if let Some(cancellation) = cancellation {
            super::super::files::check_cancel(cancellation)?;
        }
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

