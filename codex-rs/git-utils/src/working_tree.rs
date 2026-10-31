//! Advisory wakeups followed by bounded, read-only Git and content observations.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use codex_file_watcher::{Subscription, WatchPath};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::historical::{ReadCancellation, read_git, read_git_command, relative_git_path};
use crate::inspection::repository_root;
use crate::operations::GitCommandControl;
use crate::status::{GitStatus, inspect_status_with_identity};

pub struct ObservedFile {
    pub path: PathBuf,
    /// Raw file bytes, not normalized Git blob content. None means missing.
    pub sha256: Option<String>,
}

#[derive(Serialize)]
pub struct WorkingTreeObservation {
    #[serde(flatten)]
    pub status: GitStatus,
    /// Scope-bound observation identity, never a coherent-snapshot attestation.
    pub state_sha256: String,
    pub change_detected: bool,
    pub rescan_required: bool,
    pub timed_out: bool,
    #[serde(skip)]
    pub files: Vec<ObservedFile>,
    #[serde(skip)]
    watch_routes: Vec<PathBuf>,
}

/// Zero timeout captures a baseline. A positive timeout subscribes BEFORE
/// capture, compares an optional earlier baseline immediately, then rechecks
/// after every wakeup. Duplicate events with identical state are not changes.
/// Overflow returns an explicit rescan observation even if the state is equal.
/// Dropping this future cancels Git children and releases native subscriptions.
pub async fn observe_working_tree(
    cwd: PathBuf,
    paths: Vec<String>,
    max_files: usize,
    baseline: Option<String>,
    timeout: Duration,
) -> Result<WorkingTreeObservation, String> {
    if paths.len() > 64 || !(1..=1000).contains(&max_files) {
        return Err(
            "working-tree observation requires at most 64 paths and max_files 1..1000".into(),
        );
    }
    if baseline
        .as_ref()
        .is_some_and(|hash| hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(
            "baseline must be a state_sha256 from the same repository and path selection".into(),
        );
    }
    let mut paths = paths
        .iter()
        .map(|path| relative_git_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    paths.dedup();
    let control = GitCommandControl::new();
    let _cancellation = ReadCancellation(control.clone());
    let context = tokio::task::spawn_blocking(move || {
        let root = repository_root(&cwd, &control)?;
        let routes = watch_routes(&root, &control)?;
        Ok::<_, String>((root, routes))
    })
    .await
    .map_err(|error| error.to_string())??;
    let (root, routes) = context;
    let mut watches = paths
        .iter()
        .map(|path| WatchPath {
            path: root.join(path),
            recursive: true,
        })
        .collect::<Vec<_>>();
    if paths.is_empty() {
        watches.push(WatchPath {
            path: root.clone(),
            recursive: true,
        });
    }
    // Nonrecursive metadata coverage avoids watching the object database. Both
    // worktree-local and common refs/reftables matter for linked worktrees.
    for directory in routes.iter().take(2) {
        watches.push(WatchPath {
            path: directory.clone(),
            recursive: false,
        });
        for name in ["refs", "reftable", "info"] {
            watches.push(WatchPath {
                path: directory.join(name),
                recursive: true,
            });
        }
    }
    watches.push(WatchPath {
        path: routes[2].clone(),
        recursive: false,
    });
    watches.push(WatchPath {
        path: root.join(".git"),
        recursive: false,
    });
    for path in &paths {
        for parent in root
            .join(path)
            .ancestors()
            .skip(1)
            .take_while(|parent| parent.starts_with(&root))
        {
            for name in [".gitignore", ".gitattributes"] {
                watches.push(WatchPath {
                    path: parent.join(name),
                    recursive: false,
                });
            }
        }
    }
    let mut subscription = if timeout.is_zero() {
        None
    } else {
        Some(
            Subscription::new(watches)
                .await
                .map_err(|error| error.to_string())?,
        )
    };
    let mut observed = capture(root.clone(), paths.clone(), max_files).await?;
    let baseline = baseline.unwrap_or_else(|| observed.state_sha256.clone());
    observed.change_detected = observed.state_sha256 != baseline;
    observed.rescan_required = !timeout.is_zero() && observed.watch_routes != routes;
    if observed.change_detected || observed.rescan_required || timeout.is_zero() {
        return Ok(observed);
    }
    // The wait budget begins after baseline acquisition. Each capture retains
    // its own bounded Git owner; events do not reset this absolute idle deadline.
    let deadline = Instant::now() + timeout;
    let subscription = subscription
        .as_mut()
        .ok_or("working-tree subscription unavailable")?;
    loop {
        let event = match tokio::time::timeout_at(deadline, subscription.recv()).await {
            Ok(event) => event.map_err(|error| error.to_string())?,
            Err(_) => {
                // Recheck even on expiry: a delayed backend must not certify
                // unchanged state from an old baseline.
                observed = capture(root.clone(), paths.clone(), max_files).await?;
                observed.change_detected = observed.state_sha256 != baseline;
                observed.timed_out = !observed.change_detected;
                return Ok(observed);
            }
        };
        observed = capture(root.clone(), paths.clone(), max_files).await?;
        observed.change_detected = observed.state_sha256 != baseline;
        observed.rescan_required = event.rescan_required || observed.watch_routes != routes;
        if observed.change_detected || observed.rescan_required {
            return Ok(observed);
        }
    }
}

async fn capture(
    root: PathBuf,
    paths: Vec<String>,
    max_files: usize,
) -> Result<WorkingTreeObservation, String> {
    let status = inspect_status_with_identity(root.clone(), paths.clone(), max_files).await?;
    let status_sha256 = status.status_sha256.clone();
    let control = GitCommandControl::new();
    let _cancellation = ReadCancellation(control.clone());
    let (state_sha256, files, routes) = tokio::task::spawn_blocking(move || {
        let mut digest = Sha256::new();
        let mut field = |bytes: &[u8]| {
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        };
        field(b"working-tree-observation-v1");
        field(root.as_os_str().as_encoded_bytes());
        field(&(paths.len() as u64).to_le_bytes());
        for path in &paths {
            field(path.as_bytes());
        }
        field(status_sha256.as_bytes());
        let routes = watch_routes(&root, &control)?;
        for route in &routes {
            field(route.as_os_str().as_encoded_bytes());
        }
        // Unborn HEAD has no object; retain its symbolic branch separately.
        let head = read_git_command(
            &root,
            &["rev-parse", "--verify", "--quiet", "HEAD"],
            1024,
            &control,
        )?;
        if !head.status.success() && head.status.code() != Some(1) {
            return Err("cannot resolve HEAD for working-tree observation".into());
        }
        field(&head.stdout);
        let symbolic = read_git_command(
            &root,
            &["symbolic-ref", "--quiet", "HEAD"],
            32 * 1024,
            &control,
        )?;
        if !symbolic.status.success() && symbolic.status.code() != Some(1) {
            return Err("cannot resolve symbolic HEAD".into());
        }
        field(&symbolic.stdout);
        field(
            read_git(
                &root,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname) %(symref)",
                ],
                8 * 1024 * 1024,
                &control,
            )?
            .as_bytes(),
        );
        let mut args = vec![
            "--literal-pathspecs",
            "ls-files",
            "--stage",
            "--cached",
            "-z",
            "--",
        ];
        args.extend(paths.iter().map(String::as_str));
        let listing = read_git(&root, &args, 8 * 1024 * 1024, &control)?;
        field(listing.as_bytes());
        let mut names = BTreeMap::new();
        for entry in listing.split_terminator('\0') {
            let (header, name) = entry.split_once('\t').ok_or("invalid index entry")?;
            let gitlink = header.starts_with("160000 ");
            relative_git_path(name)?;
            names.insert(name.to_owned(), gitlink);
            if names.len() > 10_000 {
                return Err(
                    "working-tree observation exceeds 10000 selected files; narrow paths".into(),
                );
            }
        }
        // Separate framing avoids interpreting an untracked filename containing
        // a tab (or resembling an index header) as a tracked entry.
        let mut args = vec![
            "--literal-pathspecs",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
        ];
        args.extend(paths.iter().map(String::as_str));
        let untracked = read_git(&root, &args, 8 * 1024 * 1024, &control)?;
        field(untracked.as_bytes());
        for name in untracked.split_terminator('\0') {
            relative_git_path(name)?;
            names.insert(name.to_owned(), false);
            if names.len() > 10_000 {
                return Err(
                    "working-tree observation exceeds 10000 selected files; narrow paths".into(),
                );
            }
        }
        let mut budget = 64 * 1024 * 1024u64;
        let mut files = Vec::new();
        for (name, gitlink) in names {
            control.check().map_err(|error| error.to_string())?;
            let path = root.join(&name);
            field(name.as_bytes());
            if gitlink {
                // Do not recurse into nested worktrees. Git status already
                // observes their checked-out commit, not dirty nested files.
                let submodule = status_entry_identity(&path, &control)?;
                field(submodule.as_bytes());
                continue;
            }
            let identity = file_identity(&path, &mut budget)?;
            field(identity.as_deref().unwrap_or("missing").as_bytes());
            files.push(ObservedFile {
                path,
                sha256: identity,
            });
        }
        Ok::<_, String>((hex(digest.finalize()), files, routes))
    })
    .await
    .map_err(|error| error.to_string())??;
    Ok(WorkingTreeObservation {
        status,
        state_sha256,
        change_detected: false,
        rescan_required: false,
        timed_out: false,
        files,
        watch_routes: routes,
    })
}

fn watch_routes(root: &Path, control: &GitCommandControl) -> Result<Vec<PathBuf>, String> {
    [
        ["rev-parse", "--absolute-git-dir"].as_slice(),
        ["rev-parse", "--git-common-dir"].as_slice(),
        ["rev-parse", "--git-path", "index"].as_slice(),
    ]
    .into_iter()
    .map(|args| {
        read_git(root, args, 32 * 1024, control)
            .map(|text| root.join(text.trim_end_matches(['\r', '\n'])))
    })
    .collect()
}

fn status_entry_identity(path: &Path, control: &GitCommandControl) -> Result<String, String> {
    if !path.join(".git").exists() {
        return Ok("uninitialized_gitlink".into());
    }
    read_git(path, &["rev-parse", "--verify", "HEAD"], 1024, control)
}

fn file_identity(path: &Path, budget: &mut u64) -> Result<Option<String>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot observe {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(|error| error.to_string())?;
        return Ok(Some(format!(
            "symlink:{}",
            hex(Sha256::digest(target.as_os_str().as_encoded_bytes()))
        )));
    }
    if metadata.is_dir() {
        return Ok(Some("directory".into()));
    }
    if !metadata.is_file() {
        return Err(format!(
            "unsupported non-file working-tree path {}; narrow paths",
            path.display()
        ));
    }
    let limit = (*budget).min(8 * 1024 * 1024);
    if metadata.len() > limit {
        return Err(format!(
            "working-tree capture exceeds 8 MiB/file or 64 MiB total at {}; narrow paths",
            path.display()
        ));
    }
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("working-tree file grew beyond capture limit".into());
    }
    *budget -= bytes.len() as u64;
    Ok(Some(hex(Sha256::digest(&bytes))))
}

pub(crate) fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
