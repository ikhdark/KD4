//! Private on-disk indirection; public rollout readers still return full records.
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use sha2::{Digest, Sha256};
use codex_protocol::protocol::{EventMsg, RolloutItem};

const MAX_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const INLINE_BYTES: usize = 8 * 1024;
pub(crate) const KIND: &str = "rollout_payload_artifact";
const MAX_CONCURRENT_PAYLOAD_SYNCS: usize = 8;

#[derive(Default)]
struct PendingSync {
    sequence: u64,
    paths: std::collections::BTreeMap<(PathBuf, PathBuf), u64>,
}

fn pending_sync() -> &'static std::sync::Mutex<PendingSync> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<PendingSync>> = std::sync::OnceLock::new();
    PENDING.get_or_init(Default::default)
}

/// Sync payloads before their rollout references. Failed barriers retain their
/// work; concurrent publications retain newer sequences for the next barrier.
pub(crate) async fn sync_payload_artifacts(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    let directory = root(path);
    let rollout = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let pending = pending_sync().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .paths.iter().filter(|((owner, _), _)| owner == &rollout)
            .map(|(path, sequence)| (path.clone(), *sequence)).collect::<Vec<_>>();
        if pending.is_empty() { return Ok(()); }
        #[cfg(test)]
        let hook = sync_test_hooks().lock().unwrap().get(&rollout).cloned();
        let parallelism = MAX_CONCURRENT_PAYLOAD_SYNCS;
        #[cfg(test)]
        let parallelism = hook.as_ref().map_or(parallelism, |hook| hook.parallelism);
        sync_payload_files(&pending, parallelism, |path| {
            #[cfg(test)]
            if let Some(hook) = &hook { (hook.before_sync)(path)?; }
            std::fs::OpenOptions::new().read(true).write(true).open(path)?.sync_all()
        })?;
        #[cfg(unix)]
        std::fs::File::open(&directory)?.sync_all()?;
        let mut registry = pending_sync().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (path, sequence) in pending {
            if registry.paths.get(&path) == Some(&sequence) {
                registry.paths.remove(&path);
            }
        }
        Ok(())
    }).await.map_err(io::Error::other)?
}

/// Independent immutable payloads can sync concurrently, but the rollout must
/// wait for every worker. No registry lock is held during I/O; cancellation of
/// the async observer leaves this blocking barrier and its acknowledgements owned.
fn sync_payload_files(
    pending: &[((PathBuf, PathBuf), u64)],
    parallelism: usize,
    sync_file: impl Fn(&Path) -> io::Result<()> + Sync,
) -> io::Result<()> {
    let workers = parallelism.clamp(1, MAX_CONCURRENT_PAYLOAD_SYNCS).min(pending.len());
    if workers <= 1 {
        return pending.iter().try_for_each(|((_, path), _)| sync_file(path));
    }
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        let mut result = Ok(());
        for chunk in pending.chunks(pending.len().div_ceil(workers)) {
            let sync_file = &sync_file;
            match std::thread::Builder::new().spawn_scoped(scope, move || {
                chunk.iter().try_for_each(|((_, path), _)| sync_file(path))
            }) {
                Ok(handle) => handles.push(handle),
                Err(error) => { result = Err(error); break; }
            }
        }
        // Join successful siblings even after a failure or panic. A failed
        // barrier acknowledges nothing, so an explicit retry is lossless.
        for handle in handles {
            let joined = handle.join().unwrap_or_else(|_| Err(io::Error::other("payload sync worker panicked")));
            if result.is_ok() { result = joined; }
        }
        result
    })
}

#[cfg(test)]
type BeforeSync = std::sync::Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct SyncTestHook {
    pub(crate) parallelism: usize,
    pub(crate) before_sync: BeforeSync,
}

#[cfg(test)]
pub(crate) fn sync_test_hooks() -> &'static std::sync::Mutex<std::collections::BTreeMap<PathBuf, SyncTestHook>> {
    static HOOKS: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeMap<PathBuf, SyncTestHook>>> = std::sync::OnceLock::new();
    HOOKS.get_or_init(Default::default)
}

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

/// Decide from the canonical item before serializing or scheduling filesystem work.
pub(crate) fn is_artifact_candidate(item: &RolloutItem) -> bool {
    match item {
        RolloutItem::SessionMeta(_) | RolloutItem::ToolManifest(_) => true,
        RolloutItem::SamplingBoundary(boundary) => boundary.timing_checkpoint.is_some(),
        RolloutItem::EventMsg(event) => matches!(
            event,
            EventMsg::PatchApplyEnd(_) | EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
        ),
        _ => false,
    }
}

/// Artifacts are content-addressed and atomically published before the referencing line.
/// Per-record fsync is deferred to the durable rollout barrier.
/// Failures fall back to the original inline record at the writer boundary.
/// The caller selects eligible items with `is_artifact_candidate`.
pub(crate) fn store_line(path: &Path, line: &[u8]) -> io::Result<Vec<u8>> {
    if line.len() < INLINE_BYTES { return Ok(line.to_vec()); }
    let mut item: serde_json::Value = serde_json::from_slice(line)?;
    // Full tool-manifest definitions are usually byte-identical across threads,
    // so the content address stores one copy for every rollout that uses them.
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
        match temporary.persist_noclobber(&destination) {
            // The destination is exactly the complete temporary file we wrote.
            Ok(_) => published_here = true,
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.error),
        }
    }
    // A preexisting object or a concurrent writer that won admission is
    // verified: never reference a partial, corrupted, or substituted object,
    // including a preexisting symlink.
    if !published_here && read_payload(&directory, &sha256, bytes.len() as u64).is_err() {
        // Repair the shared directory entry atomically, without truncating it
        // or following a substituted symlink.
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        temporary.write_all(&bytes)?;
        temporary.persist(&destination).map_err(|error| error.error)?;
    }
    let mut pending = pending_sync().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    pending.sequence = pending.sequence.saturating_add(1);
    let sequence = pending.sequence;
    pending.paths.insert((path.to_path_buf(), destination), sequence);
    drop(pending);
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

    #[test]
    fn parallel_payload_sync_is_bounded_and_joins_failed_or_panicking_siblings() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for failure in ["none", "error", "panic"] {
            let pending = (0..8).map(|i| ((PathBuf::new(), PathBuf::from(i.to_string())), 0)).collect::<Vec<_>>();
            let barrier = std::sync::Barrier::new(8);
            let active = AtomicUsize::new(0);
            let peak = AtomicUsize::new(0);
            let completed = AtomicUsize::new(0);
            let result = sync_payload_files(&pending, usize::MAX, |path| {
                peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                barrier.wait();
                if path == Path::new("0") && failure != "none" {
                    active.fetch_sub(1, Ordering::SeqCst);
                    if failure == "panic" { panic!("injected worker panic"); }
                    return Err(io::Error::other("injected sync failure"));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                completed.fetch_add(1, Ordering::SeqCst);
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            });
            assert_eq!(result.is_ok(), failure == "none");
            assert_eq!(peak.load(Ordering::SeqCst), MAX_CONCURRENT_PAYLOAD_SYNCS);
            assert_eq!(active.load(Ordering::SeqCst), 0, "all started siblings must settle");
            assert_eq!(completed.load(Ordering::SeqCst), if failure == "none" { 8 } else { 7 });
        }
        let pending = (0..65).map(|i| ((PathBuf::new(), PathBuf::from(i.to_string())), 0)).collect::<Vec<_>>();
        let completed = AtomicUsize::new(0);
        // More files than the limit must not add workers, whatever the caller requests.
        let workers = std::sync::Mutex::new(std::collections::HashSet::<std::thread::ThreadId>::new());
        sync_payload_files(&pending, usize::MAX, |_| {
            workers.lock().unwrap().insert(std::thread::current().id());
            completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }).unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), pending.len());
        assert_eq!(workers.into_inner().unwrap().len(), MAX_CONCURRENT_PAYLOAD_SYNCS);
        sync_payload_files(&[], 8, |_| panic!("empty barrier must not perform I/O")).unwrap();
    }

    #[tokio::test]
    async fn cancelled_payload_barrier_keeps_ownership_and_newer_publications() {
        use std::sync::{Arc, Condvar, Mutex};
        use std::time::Duration;
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("rollout.jsonl");
        let lines = (0..8).map(|i| serde_json::to_vec(&serde_json::json!({
            "type":"event_msg", "payload":{"index":i,"saved":"exact bytes".repeat(2000)}
        })).unwrap()).collect::<Vec<_>>();
        for line in &lines { store_line(&path, line).unwrap(); }
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let blocked = Arc::clone(&gate);
        let (entered, started) = tokio::sync::oneshot::channel();
        let entered = Mutex::new(Some(entered));
        sync_test_hooks().lock().unwrap().insert(path.clone(), SyncTestHook {
            parallelism: 8,
            before_sync: Arc::new(move |_| {
                if let Some(entered) = entered.lock().unwrap().take() { let _ = entered.send(()); }
                let released = blocked.1.wait_timeout_while(blocked.0.lock().unwrap(), Duration::from_secs(5), |value| !*value).unwrap();
                if !*released.0 { return Err(io::Error::other("test did not release blocked storage")); }
                Ok(())
            }),
        });
        let observed_path = path.clone();
        let observer = tokio::spawn(async move { sync_payload_artifacts(&observed_path).await });
        tokio::time::timeout(Duration::from_secs(2), started).await.unwrap().unwrap();
        observer.abort();
        assert!(observer.await.unwrap_err().is_cancelled());
        // No registry mutex is held by stalled I/O. Another rollout completes,
        // and a repeated publication gets a newer sequence during the barrier.
        let other = home.path().join("other.jsonl");
        store_line(&other, &lines[0]).unwrap();
        tokio::time::timeout(Duration::from_secs(2), sync_payload_artifacts(&other)).await.unwrap().unwrap();
        store_line(&path, &lines[0]).unwrap();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let remaining = pending_sync().lock().unwrap().paths.keys().filter(|(owner, _)| owner == &path).count();
                if remaining == 1 { break; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("cancelled observer must not cancel acknowledgement of its old prefix");
        sync_test_hooks().lock().unwrap().remove(&path);
        sync_payload_artifacts(&path).await.unwrap();
        assert!(!pending_sync().lock().unwrap().paths.keys().any(|(owner, _)| owner == &path));
    }

    #[tokio::test]
    async fn durable_barrier_rejects_missing_payload_and_retains_retry_work() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let item = serde_json::json!({"type":"event_msg", "payload":{
            "type":"patch_apply_end", "saved":"undo bytes".repeat(2000)
        }});
        let line = serde_json::to_vec(&item).unwrap();
        let reference: serde_json::Value = serde_json::from_slice(&store_line(&path, &line).unwrap()).unwrap();
        let blob = root(&path).join(format!("{}.json", reference["payload"]["sha256"].as_str().unwrap()));
        let bytes = std::fs::read(&blob).unwrap();
        // Fault between publication and the rollout durability barrier.
        std::fs::remove_file(&blob).unwrap();
        assert_eq!(sync_payload_artifacts(&path).await.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(pending_sync().lock().unwrap().paths.keys().any(|(owner, _)| owner == &path));
        std::fs::write(&blob, bytes).unwrap();
        sync_payload_artifacts(&path).await.unwrap();
        assert!(!pending_sync().lock().unwrap().paths.keys().any(|(owner, _)| owner == &path));
    }

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
        let invalid = reader.next_line().await.unwrap().unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&invalid).is_err());
        let repaired = store_line(&path, &serde_json::to_vec(&item).unwrap()).unwrap();
        let restored: serde_json::Value = serde_json::from_str(
            &hydrate_line(&path, String::from_utf8(repaired).unwrap()).unwrap(),
        ).unwrap();
        assert_eq!(restored, item);
    }
}
