//! Independent index/worktree views from Git's NUL-delimited porcelain status.
use std::path::PathBuf;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::historical::ReadCancellation;
use crate::historical::disabled_filter_config;
use crate::historical::read_git_command_with;
use crate::historical::relative_git_path;
use crate::inspection::MAX_INSPECTION_ENTRIES;
use crate::inspection::bounded_warnings;
use crate::inspection::private_index_copy;
use crate::inspection::repository_root;
use crate::operations::GitCommandControl;

#[derive(Clone, Serialize)]
pub struct GitStatusEntry {
    /// Destination/current path. Porcelain -z reports it before the source.
    pub path: String,
    pub original_path: Option<String>,
    /// The exact X and Y columns; a space means unchanged, not unknown.
    pub index_status: String,
    pub worktree_status: String,
}

#[derive(Default, Serialize)]
pub struct GitStatusView {
    pub total_files: usize,
    pub files: Vec<GitStatusEntry>,
    pub truncated: bool,
}

impl GitStatusView {
    fn push(&mut self, entry: &GitStatusEntry, max_files: usize) {
        self.total_files += 1;
        if self.files.len() < max_files {
            self.files.push(entry.clone());
        } else {
            self.truncated = true;
        }
    }
}

#[derive(Serialize)]
pub struct GitStatus {
    #[serde(skip)]
    pub(crate) status_sha256: String,
    pub repository_root: PathBuf,
    pub paths: Vec<String>,
    /// Unique porcelain records, not the sum of the overlapping views.
    pub total_files: usize,
    pub staged: GitStatusView,
    pub unstaged: GitStatusView,
    pub untracked: GitStatusView,
    pub conflicted: GitStatusView,
    pub coherent_snapshot_verified: bool,
    pub warnings: String,
}

/// Capture status without allowing Git to refresh the caller's index. Reuse
/// the bounded read/process owner and cancel its blocking work on future drop.
pub async fn inspect_status(
    cwd: PathBuf,
    paths: Vec<String>,
    max_files: usize,
) -> Result<GitStatus, String> {
    capture_status(cwd, paths, max_files, false, false).await
}

pub(crate) async fn inspect_status_with_identity(cwd: PathBuf, paths: Vec<String>, max_files: usize) -> Result<GitStatus, String> {
    capture_status(cwd, paths, max_files, false, true).await
}

/// Display metadata must not run repository filters or refresh the live index
/// before an otherwise read-only tool executes. Keep metadata's ordinary
/// untracked/submodule configuration instead of expanding the native tool view.
pub(crate) async fn has_changes(cwd: PathBuf) -> Result<bool, String> {
    Ok(capture_status(cwd, Vec::new(), 1, true, false).await?.total_files != 0)
}

async fn capture_status(
    cwd: PathBuf,
    paths: Vec<String>,
    max_files: usize,
    metadata_only: bool,
    capture_identity: bool,
) -> Result<GitStatus, String> {
    if !(1..=MAX_INSPECTION_ENTRIES).contains(&max_files) || paths.len() > 64 {
        return Err("Git status requires max_files 1..1000 and at most 64 paths".into());
    }
    let paths = paths
        .iter()
        .map(|path| relative_git_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    let control = GitCommandControl::new();
    let _cancellation = ReadCancellation(control.clone());
    tokio::task::spawn_blocking(move || {
        let root = repository_root(&cwd, &control)?;
        let filters = disabled_filter_config(&root, &control)?;
        let (_directory, index) = private_index_copy(&root, &control)?;
        let mut args = vec![
            "--literal-pathspecs",
            "-c",
            "core.splitIndex=false",
            "-c",
            "core.safecrlf=false",
            "-c",
            "diff.renameLimit=1000",
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored=no",
            "--find-renames=50%",
        ];
        if !metadata_only {
            args.extend(["--untracked-files=all", "--ignore-submodules=dirty"]);
        }
        args.push("--");
        args.extend(paths.iter().map(String::as_str));
        let output = read_git_command_with(
            &root,
            &args,
            Some(&index),
            &filters,
            8 * 1024 * 1024,
            &control,
        )?;
        if !output.status.success() {
            return Err(format!(
                "Git status failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let entries = parse_status(&output.stdout)?;
        let mut result = GitStatus {
            status_sha256: if capture_identity { crate::working_tree::hex(Sha256::digest(&output.stdout)) } else { String::new() },
            repository_root: root,
            paths,
            total_files: entries.len(),
            staged: GitStatusView::default(),
            unstaged: GitStatusView::default(),
            untracked: GitStatusView::default(),
            conflicted: GitStatusView::default(),
            coherent_snapshot_verified: false,
            warnings: bounded_warnings(&output.stderr),
        };
        for entry in entries {
            let xy = (entry.index_status.as_str(), entry.worktree_status.as_str());
            if xy == ("?", "?") {
                result.untracked.push(&entry, max_files);
            } else if matches!(
                xy,
                ("D", "D")
                    | ("A", "U")
                    | ("U", "D")
                    | ("U", "A")
                    | ("D", "U")
                    | ("A", "A")
                    | ("U", "U")
            ) {
                result.conflicted.push(&entry, max_files);
            } else {
                if entry.index_status != " " {
                    result.staged.push(&entry, max_files);
                }
                if entry.worktree_status != " " {
                    result.unstaged.push(&entry, max_files);
                }
            }
        }
        Ok(result)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn parse_status(bytes: &[u8]) -> Result<Vec<GitStatusEntry>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "Git status requires UTF-8 paths")?;
    let text = text
        .strip_suffix('\0')
        .ok_or("unterminated Git status record")?;
    let mut records = text.split('\0');
    let mut entries = Vec::new();
    while let Some(record) = records.next() {
        let header = record.as_bytes();
        if header.len() < 4
            || header[2] != b' '
            || !header[..2].iter().all(|byte| b" MADRCUT?".contains(byte))
            || header[..2] == *b"  "
            || (header[..2].contains(&b'?') && header[..2] != *b"??")
        {
            return Err("invalid Git porcelain status record".into());
        }
        let original_path = if header[..2].iter().any(|byte| matches!(byte, b'R' | b'C')) {
            Some(
                records
                    .next()
                    .filter(|path| !path.is_empty())
                    .ok_or("missing Git status rename/copy source path")?
                    .to_string(),
            )
        } else {
            None
        };
        entries.push(GitStatusEntry {
            path: record[3..].to_string(),
            original_path,
            index_status: (header[0] as char).to_string(),
            worktree_status: (header[1] as char).to_string(),
        });
    }
    Ok(entries)
}
