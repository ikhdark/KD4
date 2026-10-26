//! Private on-disk indirection; public rollout readers still return full records.
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use sha2::Digest;
use sha2::Sha256;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

const MAX_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const INLINE_BYTES: usize = 8 * 1024;
pub(crate) const KIND: &str = "rollout_payload_artifact";
/// Directory beside the session collections that holds referenced bodies.
pub(crate) const PAYLOAD_DIRECTORY: &str = "rollout-payloads";
const MAX_CONCURRENT_PAYLOAD_SYNCS: usize = 8;

/// A reference is the small fixed-shape record `store_value` writes, always far
/// below the inline threshold. A longer line that only mentions the kind (tool
/// output quoting this source, for example) is ordinary history: classify it
/// without a JSON parse or a blocking-pool hop.
pub(crate) fn may_be_reference(line: &str) -> bool {
    line.len() < INLINE_BYTES && line.contains(KIND)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadDependencyVersion {
    pub(crate) path: PathBuf,
    stamp: Option<(
        u64,
        Option<std::time::SystemTime>,
        Option<std::time::SystemTime>,
    )>,
}

fn dependency_stamp(
    path: &Path,
) -> Option<(
    u64,
    Option<std::time::SystemTime>,
    Option<std::time::SystemTime>,
)> {
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .map(|metadata| {
            (
                metadata.len(),
                metadata.modified().ok(),
                metadata.created().ok(),
            )
        })
}

/// Observe a named artifact before the canonical reader attempts hydration.
pub(crate) fn named_payload_version(path: &Path, line: &str) -> Option<PayloadDependencyVersion> {
    let record: serde_json::Value = serde_json::from_str(line).ok()?;
    if record["type"] != KIND {
        return None;
    }
    let hash = record["payload"]["sha256"].as_str().filter(|hash| {
        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    })?;
    let path = root(path).join(format!("{hash}.json"));
    Some(PayloadDependencyVersion {
        stamp: dependency_stamp(&path),
        path,
    })
}

/// Cold-cache reference discovery only; verification remains in the canonical
/// hydrator. Warm pages stat these dependencies without replaying history.
pub async fn payload_dependency_versions(path: &Path) -> io::Result<Vec<PayloadDependencyVersion>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let file = std::fs::File::open(&path)?;
        let input: Box<dyn Read> = if path.extension().is_some_and(|extension| extension == "zst") {
            Box::new(zstd::stream::read::Decoder::new(file)?)
        } else {
            Box::new(file)
        };
        let mut reader = std::io::BufReader::new(input);
        let mut paths = std::collections::BTreeSet::new();
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            if !may_be_reference(&line) {
                continue;
            }
            let record: serde_json::Value =
                serde_json::from_str(&line).map_err(io::Error::other)?;
            if record["type"] != KIND {
                continue;
            }
            let hash = record["payload"]["sha256"]
                .as_str()
                .filter(|hash| {
                    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .ok_or_else(|| io::Error::other("invalid rollout payload digest"))?;
            paths.insert(root(&path).join(format!("{hash}.json")));
        }
        Ok(paths
            .into_iter()
            .map(|path| PayloadDependencyVersion {
                stamp: dependency_stamp(&path),
                path,
            })
            .collect())
    })
    .await
    .map_err(io::Error::other)?
}

pub async fn payload_dependencies_unchanged(versions: &[PayloadDependencyVersion]) -> bool {
    let versions = versions.to_vec();
    tokio::task::spawn_blocking(move || {
        versions.iter().all(|version| {
            version.stamp.is_some() && dependency_stamp(&version.path) == version.stamp
        })
    })
    .await
    .unwrap_or(false)
}

/// Stamps every payload a rollout names, so a known payload gap can be reused.
/// Unlike [`payload_dependency_versions`], records that cannot name a payload
/// are skipped instead of failing the scan: no artifact change repairs them.
pub async fn named_payload_versions(path: &Path) -> io::Result<Vec<PayloadDependencyVersion>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let file = std::fs::File::open(&path)?;
        let input: Box<dyn Read> = if path.extension().is_some_and(|extension| extension == "zst") {
            Box::new(zstd::stream::read::Decoder::new(file)?)
        } else {
            Box::new(file)
        };
        let mut reader = std::io::BufReader::new(input);
        let mut paths = std::collections::BTreeSet::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let Ok(text) = std::str::from_utf8(&line) else {
                continue;
            };
            if !may_be_reference(text) {
                continue;
            }
            let Ok(record) = serde_json::from_str::<serde_json::Value>(text) else {
                continue;
            };
            if record["type"] != KIND {
                continue;
            }
            if let Some(hash) = record["payload"]["sha256"].as_str().filter(|hash| {
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                paths.insert(root(&path).join(format!("{hash}.json")));
            }
        }
        Ok(paths
            .into_iter()
            .map(|path| PayloadDependencyVersion {
                stamp: dependency_stamp(&path),
                path,
            })
            .collect())
    })
    .await
    .map_err(io::Error::other)?
}

/// Unlike [`payload_dependencies_unchanged`], a payload that was missing still
/// matches while it stays missing: a known gap is read again only once a named
/// payload appears, is replaced, or is repaired.
pub async fn payload_versions_match(versions: &[PayloadDependencyVersion]) -> bool {
    let versions = versions.to_vec();
    tokio::task::spawn_blocking(move || {
        versions
            .iter()
            .all(|version| dependency_stamp(&version.path) == version.stamp)
    })
    .await
    .unwrap_or(false)
}

#[derive(Default)]
struct PendingSync {
    sequence: u64,
    paths: std::collections::BTreeMap<(PathBuf, PathBuf), u64>,
    owners: std::collections::BTreeMap<PathBuf, usize>,
}

fn pending_sync() -> &'static std::sync::Mutex<PendingSync> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<PendingSync>> = std::sync::OnceLock::new();
    PENDING.get_or_init(Default::default)
}

/// Custody spans handle recovery and accepted blocking publications. A successor
/// writer re-enrolls persisted references; a failed barrier keeps this owner alive.
pub(crate) struct PayloadSyncOwner {
    rollout: PathBuf,
}

impl PayloadSyncOwner {
    pub(crate) fn new(path: &Path) -> std::sync::Arc<Self> {
        let rollout = crate::compression::plain_rollout_path(path);
        let mut pending = pending_sync()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *pending.owners.entry(rollout.clone()).or_default() += 1;
        std::sync::Arc::new(Self { rollout })
    }
}

impl Drop for PayloadSyncOwner {
    fn drop(&mut self) {
        let mut pending = pending_sync()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(owners) = pending.owners.get_mut(&self.rollout) else {
            return;
        };
        *owners -= 1;
        if *owners != 0 {
            return;
        }
        pending.owners.remove(&self.rollout);
        let keys = pending.paths.range((self.rollout.clone(), PathBuf::new())..)
            .take_while(|((owner, _), _)| owner == &self.rollout)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            pending.paths.remove(&key);
        }
    }
}

/// Sync payloads before their rollout references. Failed barriers retain their
/// work; concurrent publications retain newer sequences for the next barrier.
pub(crate) async fn sync_payload_artifacts(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    let directory = root(path);
    let rollout = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        // Keys order by owner first, so a barrier visits only its own entries rather
        // than every rollout's retained work.
        let pending = pending_sync()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paths
            .range((rollout.clone(), PathBuf::new())..)
            .take_while(|((owner, _), _)| owner == &rollout)
            .map(|(path, sequence)| (path.clone(), *sequence))
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Ok(());
        }
        let parallelism = MAX_CONCURRENT_PAYLOAD_SYNCS;
        sync_payload_files(&pending, parallelism, |path| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?
                .sync_all()
        })?;
        #[cfg(unix)]
        std::fs::File::open(&directory)?.sync_all()?;
        let mut registry = pending_sync()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (path, sequence) in pending {
            if registry.paths.get(&path) == Some(&sequence) {
                registry.paths.remove(&path);
            }
        }
        Ok(())
    })
    .await
    .map_err(io::Error::other)?
}

/// Independent immutable payloads can sync concurrently, but the rollout must
/// wait for every worker. No registry lock is held during I/O; cancellation of
/// the async observer leaves this blocking barrier and its acknowledgements owned.
fn sync_payload_files(
    pending: &[((PathBuf, PathBuf), u64)],
    parallelism: usize,
    sync_file: impl Fn(&Path) -> io::Result<()> + Sync,
) -> io::Result<()> {
    let workers = parallelism
        .clamp(1, MAX_CONCURRENT_PAYLOAD_SYNCS)
        .min(pending.len());
    if workers <= 1 {
        return pending
            .iter()
            .try_for_each(|((_, path), _)| sync_file(path));
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
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        // Join successful siblings even after a failure or panic. A failed
        // barrier acknowledges nothing, so an explicit retry is lossless.
        for handle in handles {
            let joined = handle
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("payload sync worker panicked")));
            if result.is_ok() {
                result = joined;
            }
        }
        result
    })
}




pub(crate) fn root(path: &Path) -> PathBuf {
    for ancestor in path.ancestors().skip(1) {
        if ancestor.file_name().is_some_and(|name| {
            name == crate::SESSIONS_SUBDIR || name == crate::ARCHIVED_SESSIONS_SUBDIR
        }) {
            return ancestor
                .parent()
                .unwrap_or(ancestor)
                .join(PAYLOAD_DIRECTORY);
        }
    }
    path.parent()
        .unwrap_or(Path::new("."))
        .join(PAYLOAD_DIRECTORY)
}

/// Held from opening an append owner through its last reference publication.
/// Ordering: retention, physical append/compression lock, append transaction.
pub(crate) fn lock_publication(path: &Path) -> io::Result<std::fs::File> {
    let directory = root(path);
    std::fs::create_dir_all(&directory)?;
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".retention.lock"))?;
    file.lock_shared()?;
    Ok(file)
}

/// Collection is deferred until every authoritative rollout root and publisher
/// participates in retention ownership. Explicit resume paths can live outside
/// the session directories, and older writers do not take the retention lock.
/// Neither a bounded directory scan nor an age cutoff proves a blob is unused.
/// Keep the optional-maintenance error explicit instead of reporting reclamation
/// success while silently accumulating retained orphan bytes.
pub async fn reclaim_payload_artifacts(home: &Path) -> io::Result<usize> {
    let directory = home.join("rollout-payloads");
    tokio::task::spawn_blocking(move || {
        reclaim_abandoned_payload_stages(&directory, std::time::SystemTime::now())
    })
    .await
    .map_err(io::Error::other)??;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "payload collection deferred: complete rollout roots and publisher ownership are not established",
    ))
}

const PAYLOAD_STAGE_PREFIX: &str = "payload-stage-v1-";
const PAYLOAD_STAGE_GRACE: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

fn payload_stage(directory: &Path) -> io::Result<tempfile::NamedTempFile> {
    let file = tempfile::Builder::new()
        .prefix(PAYLOAD_STAGE_PREFIX)
        .suffix(".pending")
        .tempfile_in(directory)?;
    // Admission precedes the first retained byte. Normal Drop still owns error
    // cleanup; after process death this recognizable, unlocked stage is collectible.
    file.as_file().try_lock().map_err(io::Error::from)?;
    Ok(file)
}

fn reclaim_abandoned_payload_stages(
    directory: &Path,
    now: std::time::SystemTime,
) -> io::Result<usize> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(tag) = name
            .to_str()
            .and_then(|name| name.strip_prefix(PAYLOAD_STAGE_PREFIX))
            .and_then(|name| name.strip_suffix(".pending"))
        else {
            continue;
        };
        if tag.is_empty() || !tag.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file()
            || !now
                .duration_since(metadata.modified()?)
                .is_ok_and(|age| age >= PAYLOAD_STAGE_GRACE)
        {
            continue;
        }
        let owner = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)?;
        match owner.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(error) => return Err(error.into()),
        }
        // Recheck after admission; never remove a fresh/replaced pathname based
        // on the inventory's age. Final CAS objects and legacy anonymous names
        // are deliberately outside this private staging contract.
        let current = std::fs::symlink_metadata(&path)?;
        let held = owner.metadata()?;
        if !current.is_file()
            || current.len() != held.len()
            || current.modified()? != held.modified()?
            || current.created().ok() != held.created().ok()
            || !now
                .duration_since(current.modified()?)
                .is_ok_and(|age| age >= PAYLOAD_STAGE_GRACE)
        {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

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
/// Failures and oversized bodies fall back to the original inline record at the writer boundary.
/// The caller selects eligible items and checks the complete JSONL byte threshold.
pub(crate) fn store_value(
    path: &Path,
    mut item: serde_json::Value,
) -> io::Result<Option<Vec<u8>>> {
    // Full tool-manifest definitions are usually byte-identical across threads,
    // so the content address stores one copy for every rollout that uses them.
    let fields = item
        .as_object_mut()
        .ok_or_else(|| io::Error::other("invalid rollout object"))?;
    let timestamp = fields.remove("timestamp");
    let format_version = fields.remove("format_version");
    let bytes = serde_json::to_vec(&item)?;
    if bytes.len() as u64 > MAX_PAYLOAD_BYTES {
        return Ok(None);
    }
    let sha256 = digest(&bytes);
    let directory = root(path);
    std::fs::create_dir_all(&directory)?;
    let destination = directory.join(format!("{sha256}.json"));
    let mut published_here = false;
    if !destination.exists() {
        let mut temporary = payload_stage(&directory)?;
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
        let mut temporary = payload_stage(&directory)?;
        temporary.write_all(&bytes)?;
        temporary
            .persist(&destination)
            .map_err(|error| error.error)?;
    }
    let mut pending = pending_sync()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pending.sequence = pending.sequence.saturating_add(1);
    let sequence = pending.sequence;
    pending
        .paths
        .insert((path.to_path_buf(), destination), sequence);
    drop(pending);
    let mut reference = serde_json::json!({
        "timestamp": timestamp, "format_version": format_version,
        "type": KIND, "payload": {"sha256": sha256, "bytes": bytes.len()}
    });
    reference["payload"]["item_type"] = item["type"].clone();
    let mut serialized = serde_json::to_vec(&reference)?;
    serialized.push(b'\n');
    Ok(Some(serialized))
}

fn read_payload(directory: &Path, sha256: &str, bytes: u64) -> io::Result<Vec<u8>> {
    if sha256.len() != 64
        || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || bytes > MAX_PAYLOAD_BYTES
    {
        return Err(io::Error::other("invalid rollout payload reference"));
    }
    let path = directory.join(format!("{sha256}.json"));
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() || metadata.len() != bytes {
        return Err(io::Error::other(
            "rollout payload is missing, replaced, or incomplete",
        ));
    }
    let mut payload = Vec::new();
    std::fs::File::open(path)?
        .take(bytes.saturating_add(1))
        .read_to_end(&mut payload)?;
    if payload.len() as u64 != bytes || digest(&payload) != sha256 {
        return Err(io::Error::other("rollout payload checksum mismatch"));
    }
    Ok(payload)
}

pub(crate) fn hydrate_line(path: &Path, line: String) -> io::Result<String> {
    hydrate_line_with_enrollment(path, line, false)
}

pub(crate) fn hydrate_line_with_enrollment(
    path: &Path,
    line: String,
    enroll: bool,
) -> io::Result<String> {
    hydrate_line_with_limit(path, line, enroll, u64::MAX)
}

pub(crate) fn hydrate_line_bounded(
    path: &Path,
    line: String,
    max_bytes: u64,
) -> io::Result<String> {
    hydrate_line_with_limit(path, line, false, max_bytes)
}

fn hydrate_line_with_limit(
    path: &Path,
    line: String,
    enroll: bool,
    max_bytes: u64,
) -> io::Result<String> {
    if !may_be_reference(&line) {
        return Ok(line);
    }
    let reference: serde_json::Value = match serde_json::from_str(&line) {
        Ok(value) => value,
        Err(_) => return Ok(line), // Preserve the existing torn-line handling.
    };
    if reference["type"] != KIND {
        return Ok(line);
    }
    let sha256 = reference["payload"]["sha256"]
        .as_str()
        .ok_or_else(|| io::Error::other("rollout payload has no hash"))?;
    let bytes = reference["payload"]["bytes"]
        .as_u64()
        .ok_or_else(|| io::Error::other("rollout payload has no byte length"))?;
    if bytes > max_bytes {
        return Err(io::Error::other(format!(
            "rollout payload exceeds the remaining {max_bytes}-byte export budget"
        )));
    }
    let mut item: serde_json::Value =
        serde_json::from_slice(&read_payload(&root(path), sha256, bytes)?)?;
    if item["type"] != reference["payload"]["item_type"] || item["type"] == KIND {
        return Err(io::Error::other("rollout payload type mismatch"));
    }
    if enroll {
        let mut pending = pending_sync()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.sequence = pending.sequence.saturating_add(1);
        let sequence = pending.sequence;
        // A cold reader observes `.jsonl.zst`, but the writer's durable barrier
        // syncs dependencies keyed by the materialized plain rollout path.
        pending.paths.insert(
            (
                crate::compression::plain_rollout_path(path),
                root(path).join(format!("{sha256}.json")),
            ),
            sequence,
        );
    }
    item["timestamp"] = reference["timestamp"].clone();
    item["format_version"] = reference["format_version"].clone();
    serde_json::to_string(&item).map_err(io::Error::other)
}

