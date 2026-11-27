//! Explicit cutover: only cooperating publishers and home-local recovery sources.
use std::cell::Cell;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// Per-pass bounds, not a removal quota. If a complete inventory does not fit,
// defer collection rather than treating an unvisited reference as absent.
const MAX_INVENTORY_ENTRIES: usize = 1_000_000;
const MAX_MARK_DEPTH: usize = 64;
const MAX_RETAINED_HASHES: usize = 1_000_000;
const MAX_MARK_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_COMPRESSED_MARK_BYTES: u64 = 64 * 1024 * 1024 * 1024;

type PayloadHash = [u8; 64];

pub fn home_retention_enabled(home: &Path) -> bool {
    let path = home.join("archive-retention.policy");
    let expected = b"local-home-only:7-days\n";
    fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() == expected.len() as u64)
        && fs::read(path).is_ok_and(|bytes| bytes == expected)
}

pub async fn collect_home_payloads(home: &Path) -> io::Result<usize> {
    let policy_home = home.to_path_buf();
    if !tokio::task::spawn_blocking(move || home_retention_enabled(&policy_home))
        .await
        .map_err(io::Error::other)?
    {
        return Ok(0);
    }
    let Some(moves) = crate::try_lock_rollout_moves(home).await? else {
        return Ok(0);
    };
    if crate::has_recoverable_delete_staging(home).await? {
        return Ok(0);
    }
    let home = home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _moves = moves;
        let home = fs::canonicalize(home)?;
        let dir = home.join(super::PAYLOAD_DIRECTORY);
        if !dir.try_exists()? {
            return Ok(0);
        }
        direct(&dir)?;
        let owner = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(".retention.lock"))?;
        let signal = dir.join(".retention-yield");
        match fs::remove_file(&signal) {
            Ok(()) => (),
            Err(err) if err.kind() == io::ErrorKind::NotFound => (),
            Err(err) => return Err(err),
        }
        match owner.try_lock() {
            Ok(()) => (),
            Err(std::fs::TryLockError::WouldBlock) => return Ok(0),
            Err(err) => return Err(err.into()),
        }
        let mut budget = InventoryBudget::new(signal);
        let retained = mark_home(&home, &dir, &mut budget)?;
        let mut garbage = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            budget.visit_entry()?;
            let name = entry.file_name();
            let Some((hash, chunk)) = name.to_str().and_then(|s| {
                s.strip_suffix(".json")
                    .map(|h| (h, false))
                    .or_else(|| s.strip_suffix(".replay-zst").map(|h| (h, true)))
            }) else {
                continue;
            };
            let Ok(hash) = <&PayloadHash>::try_from(hash.as_bytes()) else {
                continue;
            };
            if hash
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
                && !retained.contains(&(*hash, chunk))
                && entry.file_type()?.is_file()
            {
                direct(&entry.path())?;
                // Keep just the bounded filename, not a full path per candidate.
                garbage.push(name);
            }
        }
        // Neither marking nor candidate enumeration can leave a partial
        // inventory eligible for deletion.
        if !home_retention_enabled(&home) {
            return Ok(0);
        }
        let mut count = 0;
        for name in garbage {
            budget.yield_if_busy()?;
            fs::remove_file(dir.join(name))?;
            count += 1;
        }
        tracing::info!(removed = count, "home payload collection finished");
        Ok(count)
    })
    .await
    .map_err(io::Error::other)?
}

fn direct(path: &Path) -> io::Result<()> {
    if fs::canonicalize(path)? != path {
        return Err(io::Error::other("indirect retention path"));
    }
    Ok(())
}

fn mark_home(
    home: &Path,
    payload_directory: &Path,
    budget: &mut InventoryBudget,
) -> io::Result<HashSet<(PayloadHash, bool)>> {
    let mut retained = HashSet::new();
    let sessions = home.join(crate::SESSIONS_SUBDIR);
    let archives = home.join(crate::ARCHIVED_SESSIONS_SUBDIR);
    // root(path) resolves loose home files to this store too. A legacy explicit
    // resume path need not have a conventional filename.
    for entry in fs::read_dir(home)? {
        let entry = entry?;
        budget.visit_entry()?;
        let p = entry.path();
        let m = fs::symlink_metadata(&p)?;
        direct(&p)?;
        if m.is_file() && (m.len() >= 64 || is_zstd(&p)) {
            mark_file(&p, &mut retained, false, budget)?;
        } else if m.is_dir() && p != payload_directory && p != sessions && p != archives {
            mark_tree(
                &p,
                MarkTree::Exports(payload_directory),
                0,
                &mut retained,
                budget,
            )?;
        }
    }
    for p in [sessions, archives] {
        if p.try_exists()? {
            mark_tree(&p, MarkTree::History, 0, &mut retained, budget)?;
        }
    }
    Ok(retained)
}

#[derive(Clone, Copy)]
enum MarkTree<'a> {
    History,
    Exports(&'a Path),
}

fn mark_tree(
    parent: &Path,
    tree: MarkTree<'_>,
    depth: usize,
    retained: &mut HashSet<(PayloadHash, bool)>,
    budget: &mut InventoryBudget,
) -> io::Result<()> {
    if depth >= MAX_MARK_DEPTH {
        return Err(incomplete_inventory("mark directory depth limit"));
    }
    budget.yield_if_busy()?;
    if matches!(tree, MarkTree::History) {
        direct(parent)?;
    }
    // Traverse depth-first without accumulating every sibling path.
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        budget.visit_entry()?;
        let p = entry.path();
        match tree {
            MarkTree::History => {
                direct(&p)?;
                let m = fs::symlink_metadata(&p)?;
                if m.is_dir() {
                    mark_tree(&p, tree, depth + 1, retained, budget)?;
                } else if m.is_file() && (m.len() >= 64 || is_zstd(&p)) {
                    mark_file(&p, retained, true, budget)?;
                }
            }
            MarkTree::Exports(payload_directory) => {
                // Saved exports can explicitly select this pool. Preserve their
                // JSONL copies without following redirected directories.
                let kind = entry.file_type()?;
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if kind.is_dir() {
                    let canonical = fs::canonicalize(&p)?;
                    if canonical == p {
                        mark_tree(&p, tree, depth + 1, retained, budget)?;
                    } else if canonical == payload_directory {
                        return Err(io::Error::other("aliased payload store"));
                    }
                } else if name.ends_with(".jsonl") || name.ends_with(".jsonl.zst") {
                    direct(&p)?;
                    mark_file(&p, retained, true, budget)?;
                } else if name == super::PAYLOAD_DIRECTORY
                    && fs::canonicalize(&p)? == payload_directory
                {
                    return Err(io::Error::other("aliased payload store"));
                }
            }
        }
    }
    Ok(())
}

fn incomplete_inventory(reason: &str) -> io::Error {
    io::Error::other(format!(
        "incomplete home payload inventory: {reason}; sweep skipped"
    ))
}

struct InventoryBudget {
    yield_signal: PathBuf,
    last_poll: Cell<Instant>,
    entries_left: usize,
    mark_bytes_left: Cell<u64>,
    compressed_bytes_left: Cell<u64>,
}

impl InventoryBudget {
    fn new(yield_signal: PathBuf) -> Self {
        Self {
            yield_signal,
            last_poll: Cell::new(Instant::now()),
            entries_left: MAX_INVENTORY_ENTRIES,
            mark_bytes_left: Cell::new(MAX_MARK_BYTES),
            compressed_bytes_left: Cell::new(MAX_COMPRESSED_MARK_BYTES),
        }
    }

    fn yield_if_busy(&self) -> io::Result<()> {
        if self.last_poll.get().elapsed() >= Duration::from_millis(50) {
            self.last_poll.set(Instant::now());
            if self.yield_signal.try_exists()? {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "foreground payload publication waiting",
                ));
            }
        }
        Ok(())
    }

    fn visit_entry(&mut self) -> io::Result<()> {
        self.yield_if_busy()?;
        self.entries_left = self
            .entries_left
            .checked_sub(1)
            .ok_or_else(|| incomplete_inventory("directory entry limit"))?;
        Ok(())
    }
}

struct MarkReader<'a, R> {
    inner: R,
    budget: &'a InventoryBudget,
    bytes_left: &'a Cell<u64>,
    limit_reason: &'static str,
}

impl<R: Read> Read for MarkReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.budget.yield_if_busy()?;
        let remaining = self.bytes_left.get();
        // Permit an EOF probe when exactly at the limit, but never present a
        // budget cutoff as EOF: the caller must not sweep a partial inventory.
        let allowed = usize::try_from(remaining.max(1)).unwrap_or(usize::MAX);
        let len = buf.len().min(allowed);
        let n = self.inner.read(&mut buf[..len])?;
        let remaining = remaining
            .checked_sub(n as u64)
            .ok_or_else(|| incomplete_inventory(self.limit_reason))?;
        self.bytes_left.set(remaining);
        Ok(n)
    }
}

fn is_zstd(path: &Path) -> bool {
    path.extension()
        .is_some_and(|s| s.eq_ignore_ascii_case("zst"))
}

fn mark_file(
    path: &Path,
    retained: &mut HashSet<(PayloadHash, bool)>,
    history: bool,
    budget: &InventoryBudget,
) -> io::Result<()> {
    let file = File::open(path)?;
    let input: Box<dyn Read> = if is_zstd(path) {
        // Limit encoded input too: a decoder can consume many bytes without
        // producing output (for example when it skips frames).
        Box::new(zstd::stream::read::Decoder::new(MarkReader {
            inner: file,
            budget,
            bytes_left: &budget.compressed_bytes_left,
            limit_reason: "compressed mark byte limit",
        })?)
    } else {
        Box::new(file)
    };
    let mut reader = BufReader::with_capacity(
        128 * 1024,
        MarkReader {
            inner: input,
            budget,
            bytes_left: &budget.mark_bytes_left,
            limit_reason: "decoded mark byte limit",
        },
    );
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = (&mut reader)
            .take(super::INLINE_BYTES as u64 + 2)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if n >= super::INLINE_BYTES + 2 {
            if line.last() != Some(&b'\n') {
                reader.skip_until(b'\n')?;
            }
            continue;
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        let text = text.trim_end_matches(['\r', '\n']);
        if !super::may_be_reference(text) {
            continue;
        }
        // A non-JSON line mentioning the tag is not a readable reference.
        let Ok(record) = serde_json::from_str::<serde_json::Value>(text) else {
            if history || path.extension().is_some_and(|s| s == "jsonl" || s == "zst") {
                return Err(io::Error::other(
                    "ambiguous payload reference; sweep skipped",
                ));
            }
            // Root siblings also include SQLite pages and pretty-printed reports.
            // Only independently decodable records can act as loose rollout references.
            continue;
        };
        if record["type"] == super::KIND {
            let mut hash = record["payload"]["sha256"]
                .as_str()
                .and_then(|s| PayloadHash::try_from(s.as_bytes()).ok())
                .filter(|hash| hash.iter().all(u8::is_ascii_hexdigit))
                .ok_or_else(|| io::Error::other("invalid payload reference; sweep skipped"))?;
            hash.make_ascii_lowercase();
            if retained.len() >= MAX_RETAINED_HASHES && !retained.contains(&(hash, false)) {
                return Err(incomplete_inventory("retained hash limit"));
            }
            retained.insert((hash, false));
            if let Some(blocks) = super::replay_chunks::manifest(&record["payload"])? {
                for (digest, _, _) in blocks {
                    let key = (
                        PayloadHash::try_from(digest.as_bytes()).map_err(io::Error::other)?,
                        true,
                    );
                    if retained.len() >= MAX_RETAINED_HASHES && !retained.contains(&key) {
                        return Err(incomplete_inventory("retained hash limit"));
                    }
                    retained.insert(key);
                }
            }
        }
    }
    Ok(())
}
