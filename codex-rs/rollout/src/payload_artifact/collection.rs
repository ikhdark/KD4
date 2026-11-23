//! Explicit cutover: only cooperating publishers and home-local recovery sources.
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

pub fn home_retention_enabled(home: &Path) -> bool {
    let path = home.join("archive-retention.policy");
    let expected = b"local-home-only:7-days\n";
    fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() == expected.len() as u64)
        && fs::read(path).is_ok_and(|bytes| bytes == expected)
}

pub async fn collect_home_payloads(home: &Path) -> io::Result<usize> {
    if !home_retention_enabled(home) {
        return Ok(0);
    }
    let Some(moves) = crate::try_lock_rollout_moves(home).await? else {
        return Ok(0);
    };
    if crate::has_recoverable_delete_staging(home).await? {
        return Ok(0);
    }
    let home = fs::canonicalize(home)?;
    tokio::task::spawn_blocking(move || {
        let _moves = moves;
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
        let mut last_poll = std::time::Instant::now();
        let mut yield_if_busy = || -> io::Result<()> {
            if last_poll.elapsed() >= std::time::Duration::from_millis(50) {
                last_poll = std::time::Instant::now();
                if signal.try_exists()? {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "foreground payload publication waiting",
                    ));
                }
            }
            Ok(())
        };
        let mut retained = HashSet::new();
        // root(path) resolves loose home files to this store too. A legacy
        // explicit resume path need not have a conventional filename.
        let mut other_dirs = Vec::new();
        for entry in fs::read_dir(&home)? {
            let entry = entry?;
            let p = entry.path();
            let m = fs::symlink_metadata(&p)?;
            direct(&p)?;
            if m.is_file() && (m.len() >= 64 || is_zstd(&p)) {
                mark_file(&p, &mut retained, false, &mut yield_if_busy)?;
            } else if m.is_dir()
                && p != dir
                && p != home.join(crate::SESSIONS_SUBDIR)
                && p != home.join(crate::ARCHIVED_SESSIONS_SUBDIR)
            {
                other_dirs.push(p);
            }
        }
        let mut dirs = Vec::new();
        for name in [crate::SESSIONS_SUBDIR, crate::ARCHIVED_SESSIONS_SUBDIR] {
            let p = home.join(name);
            if p.try_exists()? {
                dirs.push(p);
            }
        }
        while let Some(p) = dirs.pop() {
            direct(&p)?;
            for entry in fs::read_dir(p)? {
                let p = entry?.path();
                direct(&p)?;
                let m = fs::symlink_metadata(&p)?;
                if m.is_dir() {
                    dirs.push(p);
                } else if m.is_file() && (m.len() >= 64 || is_zstd(&p)) {
                    mark_file(&p, &mut retained, true, &mut yield_if_busy)?;
                }
            }
        }
        // Saved exports inside the home can explicitly select this payload pool.
        // Mark JSONL copies too; never recurse through redirected directories.
        while let Some(parent) = other_dirs.pop() {
            for entry in fs::read_dir(parent)? {
                yield_if_busy()?;
                let entry = entry?;
                let p = entry.path();
                let kind = entry.file_type()?;
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if kind.is_dir() {
                    let canonical = fs::canonicalize(&p)?;
                    if canonical == p {
                        other_dirs.push(p);
                    } else if canonical == dir {
                        return Err(io::Error::other("aliased payload store"));
                    }
                } else if name.ends_with(".jsonl") || name.ends_with(".jsonl.zst") {
                    direct(&p)?;
                    mark_file(&p, &mut retained, true, &mut yield_if_busy)?;
                } else if name == super::PAYLOAD_DIRECTORY && fs::canonicalize(&p)? == dir {
                    return Err(io::Error::other("aliased payload store"));
                }
            }
        }
        let mut garbage = Vec::new();
        for entry in fs::read_dir(&dir)? {
            yield_if_busy()?;
            let entry = entry?;
            let name = entry.file_name();
            let Some(hash) = name.to_str().and_then(|s| s.strip_suffix(".json")) else {
                continue;
            };
            if hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && !retained.contains(hash)
                && entry.file_type()?.is_file()
            {
                let p = entry.path();
                direct(&p)?;
                garbage.push(p);
            }
        }
        if !home_retention_enabled(&home) {
            return Ok(0);
        }
        let mut count = 0;
        for p in garbage {
            yield_if_busy()?;
            fs::remove_file(p)?;
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

struct ResponsiveReader<'a> {
    inner: Box<dyn Read>,
    poll: &'a mut dyn FnMut() -> io::Result<()>,
}
impl Read for ResponsiveReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (self.poll)()?;
        self.inner.read(buf)
    }
}

fn is_zstd(path: &Path) -> bool {
    path.extension()
        .is_some_and(|s| s.eq_ignore_ascii_case("zst"))
}

fn mark_file(
    path: &Path,
    retained: &mut HashSet<String>,
    history: bool,
    poll: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let file = File::open(path)?;
    let input: Box<dyn Read> = if is_zstd(path) {
        Box::new(zstd::stream::read::Decoder::new(file)?)
    } else {
        Box::new(file)
    };
    let mut reader = BufReader::with_capacity(128 * 1024, ResponsiveReader { inner: input, poll });
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
            let hash = record["payload"]["sha256"]
                .as_str()
                .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or_else(|| io::Error::other("invalid payload reference; sweep skipped"))?;
            retained.insert(hash.to_ascii_lowercase());
        }
    }
    Ok(())
}
