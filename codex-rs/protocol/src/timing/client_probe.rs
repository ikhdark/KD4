//! Default-off, bounded native client observations for KDR. No message bodies.
//!
//! Records use the existing process clock domain, not a second wall/QPC clock.
//! Missing files, partial writes, overflow (stage 0) and missing endpoints fail
//! qualification; none of them is evidence that an operation did not occur.
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

const MAX_BYTES: u64 = 8 * 1024 * 1024;
struct Journal {
    file: File,
    sequence: u64,
    bytes: u64,
    failed: bool,
}

fn journal() -> Option<&'static Mutex<Journal>> {
    static JOURNAL: OnceLock<Option<Mutex<Journal>>> = OnceLock::new();
    JOURNAL
        .get_or_init(|| {
            if std::env::var("CODEX_CLIENT_OBSERVATIONS").as_deref() != Ok("1") {
                return None;
            }
            let directory = PathBuf::from(std::env::var_os("CODEX_CLIENT_OBSERVATIONS_DIR")?);
            if !directory.is_absolute() || !directory.is_dir() {
                return None;
            }
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(format!("client-probe-{}.jsonl", std::process::id())))
                .ok()?;
            Some(Mutex::new(Journal {
                file,
                sequence: 0,
                bytes: 0,
                failed: false,
            }))
        })
        .as_ref()
}

pub fn enabled() -> bool {
    journal().is_some()
}

/// FNV-1a identity only, never a content digest or proof of uniqueness.
pub fn key(value: &str) -> u64 {
    value
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

pub fn instance() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    if enabled() {
        NEXT.fetch_add(1, Ordering::Relaxed)
    } else {
        0
    }
}

/// v1: version, PID, domain key, sequence, process ns, stage, connection,
/// thread key, turn key, operation key, and four owner-defined numeric values.
/// Stages 100/101/102 = runtime ready/shutdown/join; 110/111/112 = facade;
/// 120 = extension warning enqueue outcome; 121 = public client receipt.
pub fn record(
    stage: u64,
    connection: u64,
    thread: u64,
    turn: u64,
    operation: u64,
    values: [u64; 4],
) {
    let Some(journal) = journal() else { return };
    // No async suspension or other owner lock is held inside this critical section.
    let Ok(mut journal) = journal.lock() else {
        return;
    };
    if journal.failed {
        return;
    }
    journal.sequence += 1;
    let mut row = [
        1,
        u64::from(std::process::id()),
        key(super::process_clock_domain_id()),
        journal.sequence,
        u64::try_from(super::process_monotonic_ns()).unwrap_or(u64::MAX),
        stage,
        connection,
        thread,
        turn,
        operation,
        values[0],
        values[1],
        values[2],
        values[3],
    ];
    let Ok(mut bytes) = serde_json::to_vec(&row) else {
        journal.failed = true;
        return;
    };
    bytes.push(b'\n');
    if journal.bytes + bytes.len() as u64 > MAX_BYTES - 512 {
        row[5] = 0;
        let Ok(mut overflow) = serde_json::to_vec(&row) else {
            journal.failed = true;
            return;
        };
        overflow.push(b'\n');
        let _ = journal.file.write_all(&overflow);
        journal.failed = true;
        return;
    }
    if journal.file.write_all(&bytes).is_err() {
        journal.failed = true;
        return;
    }
    journal.bytes += bytes.len() as u64;
}
