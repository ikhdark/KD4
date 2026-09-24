//! Warm, leased output lanes for captured workspace validation.
use anyhow::Context;
use fs2::FileExt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

const LANES: usize = 4;

fn open_lane(base: &Path, index: usize) -> anyhow::Result<(PathBuf, fs::File)> {
    let lane = if index == 0 {
        base.to_path_buf()
    } else {
        base.with_file_name(format!(
            "{}-{index}",
            base.file_name().context("lane name")?.to_string_lossy()
        ))
    };
    fs::create_dir_all(&lane)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lane.join("lease.lock"))?;
    Ok((lane, lock))
}

/// Leases the first idle output lane under `base` without waiting; `None`
/// when every lane is busy. The lease lasts until the returned file is dropped.
pub fn try_acquire_lane(base: &Path) -> anyhow::Result<Option<(PathBuf, fs::File, usize)>> {
    for index in 0..LANES {
        let (lane, lock) = open_lane(base, index)?;
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(Some((lane, lock, index))),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

/// Marks every file of a captured source copy as modified now. Cargo judges
/// path-package freshness by mtime and hashes workspace members relative to
/// their root, so a copy captured before another snapshot's build started in
/// the same lane would otherwise reuse that build's artifacts. Call it only
/// while holding the lane's lease, after every earlier build there finished.
pub fn refresh_snapshot_mtimes(root: &Path) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    pending.push(entry.path());
                }
            } else if kind.is_file() {
                let mut options = fs::OpenOptions::new();
                // Timestamps need only attribute access; captured copies keep
                // their source permissions and may be read-only.
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
                    options.access_mode(FILE_WRITE_ATTRIBUTES);
                }
                #[cfg(not(windows))]
                options.read(true);
                options
                    .open(entry.path())?
                    .set_modified(now)
                    .with_context(|| format!("refresh {}", entry.path().display()))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonblocking_lane_lease_reports_saturation_and_reopens_released_lanes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("lane");
        let mut leases = (0..LANES)
            .map(|expected| {
                let (lane, lease, index) = try_acquire_lane(&base).unwrap().expect("idle lane");
                assert_eq!(index, expected);
                (lane, lease)
            })
            .collect::<Vec<_>>();
        assert!(
            try_acquire_lane(&base).unwrap().is_none(),
            "a saturated origin must not wait for a lane"
        );
        let (released, lease) = leases.remove(2);
        drop(lease);
        let (lane, _lease, index) = try_acquire_lane(&base).unwrap().expect("released lane");
        assert_eq!((lane, index), (released, 2));
    }

    #[test]
    fn refreshed_snapshot_is_rebuilt_in_a_lane_built_from_another_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let lane = dir.path().join("lane");
        let snapshot = |name: &str, value: u32| {
            let root = dir.path().join(name).join("work");
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(
                root.join("Cargo.toml"),
                "[package]\nname='lane_probe'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
            )
            .unwrap();
            fs::write(
                root.join("src/lib.rs"),
                format!("#[test] fn probe() {{ println!(\"probe value {value}\"); }}\n"),
            )
            .unwrap();
            root
        };
        let run = |root: &Path| {
            let output = crate::command("cargo")
                .args(["test", "--offline", "--lib", "--", "--nocapture"])
                .current_dir(root)
                .env("CARGO_TARGET_DIR", lane.join("target"))
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        // The second snapshot was captured before the first build started in
        // the shared lane; Cargo hashes both copies identically.
        let second = snapshot("second", 2);
        let captured = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        for file in ["Cargo.toml", "src/lib.rs"] {
            fs::File::options()
                .write(true)
                .open(second.join(file))
                .unwrap()
                .set_modified(captured)
                .unwrap();
        }
        let first = snapshot("first", 1);
        assert!(run(&first).contains("probe value 1"));

        refresh_snapshot_mtimes(&second).unwrap();
        let output = run(&second);
        assert!(
            output.contains("probe value 2"),
            "the lane must not serve the other snapshot's build: {output}"
        );
    }

    #[test]
    fn snapshot_mtime_refresh_covers_read_only_sources_but_not_git_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let source = dir.path().join("src/nested/lib.rs");
        let read_only = dir.path().join("Cargo.toml");
        let metadata = dir.path().join(".git/index");
        for path in [&source, &read_only, &metadata] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "contents").unwrap();
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let mut permissions = fs::metadata(&read_only).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&read_only, permissions).unwrap();
        let before = std::time::SystemTime::now();

        refresh_snapshot_mtimes(dir.path()).unwrap();

        let modified = |path: &Path| fs::metadata(path).unwrap().modified().unwrap();
        assert!(modified(&source) >= before);
        assert!(modified(&read_only) >= before);
        assert!(fs::metadata(&read_only).unwrap().permissions().readonly());
        assert!(
            modified(&metadata) < before,
            "Git metadata is not a build input"
        );
        let mut permissions = fs::metadata(&read_only).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&read_only, permissions).unwrap();
    }
}
