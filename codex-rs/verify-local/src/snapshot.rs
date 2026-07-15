use crate::model::RawPath;
use crate::model::RepositorySnapshot;
use crate::model::SnapshotRecord;
use crate::model::SnapshotSource;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitComparisonMode {
    Direct,
    PullRequestMergeBase,
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("failed to canonicalize repository root {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Git invocation failed ({argv}): {message}")]
    Git { argv: String, message: String },
    #[error("invalid commit identifier: {0}")]
    InvalidCommitId(String),
    #[error("commit did not resolve to exactly one object: {0}")]
    InvalidCommitObject(String),
    #[error("merge-base resolution returned {count} objects")]
    AmbiguousMergeBase { count: usize },
    #[error("malformed Git record: {0}")]
    Malformed(String),
    #[error("unsupported Git copy record")]
    UnsupportedCopy,
    #[error("invalid repository path: {0}")]
    InvalidPath(String),
}

impl RepositorySnapshot {
    pub fn from_explicit_paths(
        repository_root: &Path,
        paths: impl IntoIterator<Item = RawPath>,
    ) -> Result<Self, SnapshotError> {
        let repository_root = canonical_root(repository_root)?;
        let mut records = paths
            .into_iter()
            .map(|path| {
                path.validate_repository_relative()
                    .map_err(SnapshotError::InvalidPath)?;
                Ok(SnapshotRecord {
                    status: "M".to_string(),
                    path,
                    original_path: None,
                    staged: false,
                    unstaged: false,
                    submodule_state: None,
                })
            })
            .collect::<Result<Vec<_>, SnapshotError>>()?;
        sort_records(&mut records);
        records.dedup_by(|left, right| {
            left.status == right.status
                && left.path == right.path
                && left.original_path == right.original_path
        });
        Ok(Self {
            repository_root: Some(repository_root),
            source: SnapshotSource::ExplicitPaths,
            records,
            complete: true,
            fallback_reasons: Vec::new(),
        })
    }

    pub fn from_worktree(repository_root: &Path) -> Result<Self, SnapshotError> {
        let repository_root = canonical_root(repository_root)?;
        let output = run_git(
            &repository_root,
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
                "--find-renames=50%",
            ],
        )?;
        let mut records = parse_porcelain_v2(&output)?;
        sort_records(&mut records);
        Ok(Self {
            repository_root: Some(repository_root),
            source: SnapshotSource::Worktree,
            records,
            complete: true,
            fallback_reasons: Vec::new(),
        })
    }

    pub fn from_commit_diff(
        repository_root: &Path,
        base: &str,
        head: &str,
        mode: CommitComparisonMode,
    ) -> Result<Self, SnapshotError> {
        let repository_root = canonical_root(repository_root)?;
        let base = resolve_commit(&repository_root, base)?;
        let head = resolve_commit(&repository_root, head)?;
        let (comparison_base, merge_base) = match mode {
            CommitComparisonMode::Direct => (base.clone(), None),
            CommitComparisonMode::PullRequestMergeBase => {
                let output = run_git(&repository_root, &["merge-base", "--all", &base, &head])?;
                let merge_base_output = String::from_utf8_lossy(&output);
                let bases = merge_base_output
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(str::trim)
                    .collect::<Vec<_>>();
                if bases.len() != 1 {
                    return Err(SnapshotError::AmbiguousMergeBase { count: bases.len() });
                }
                let resolved = resolve_commit(&repository_root, bases[0])?;
                (resolved.clone(), Some(resolved))
            }
        };
        let output = run_git(
            &repository_root,
            &[
                "diff",
                "--name-status",
                "-z",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                "--find-renames=50%",
                &comparison_base,
                &head,
                "--",
            ],
        )?;
        let mut records = parse_name_status(&output)?;
        sort_records(&mut records);
        Ok(Self {
            repository_root: Some(repository_root),
            source: SnapshotSource::CommitDiff {
                base,
                head,
                merge_base,
            },
            records,
            complete: true,
            fallback_reasons: Vec::new(),
        })
    }

    pub fn full_fallback(repository_root: &Path, reason: impl Into<String>) -> Self {
        Self {
            repository_root: fs::canonicalize(repository_root).ok(),
            source: SnapshotSource::Worktree,
            records: Vec::new(),
            complete: false,
            fallback_reasons: vec![reason.into()],
        }
    }
}

fn canonical_root(path: &Path) -> Result<PathBuf, SnapshotError> {
    fs::canonicalize(path).map_err(|source| SnapshotError::Canonicalize {
        path: path.to_path_buf(),
        source,
    })
}

fn resolve_commit(repository_root: &Path, value: &str) -> Result<String, SnapshotError> {
    if value.len() < 7 || value.len() > 64 || !value.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return Err(SnapshotError::InvalidCommitId(value.to_string()));
    }
    let expression = format!("{value}^{{commit}}");
    let output = run_git(
        repository_root,
        &["rev-parse", "--verify", "--end-of-options", &expression],
    )?;
    let resolved_output = String::from_utf8_lossy(&output);
    let lines = resolved_output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::trim)
        .collect::<Vec<_>>();
    if lines.len() != 1
        || !(40..=64).contains(&lines[0].len())
        || !lines[0].as_bytes().iter().all(u8::is_ascii_hexdigit)
    {
        return Err(SnapshotError::InvalidCommitObject(value.to_string()));
    }
    Ok(lines[0].to_ascii_lowercase())
}

fn run_git(repository_root: &Path, args: &[&str]) -> Result<Vec<u8>, SnapshotError> {
    let mut command = Command::new("git");
    command
        .current_dir(repository_root)
        .arg("--no-pager")
        .args(["-c", "color.ui=false"])
        .args(["-c", "core.quotepath=false"])
        .args(["-c", "core.pager=cat"])
        .args(["-c", "diff.external="])
        .args(["-c", "diff.renamelimit=0"])
        .args(args)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_EXTERNAL_DIFF", "")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
    let output = command.output().map_err(|source| SnapshotError::Git {
        argv: display_argv(args),
        message: source.to_string(),
    })?;
    if !output.status.success() {
        return Err(SnapshotError::Git {
            argv: display_argv(args),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(output.stdout)
}

fn display_argv(args: &[&str]) -> String {
    std::iter::once("git")
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_porcelain_v2(bytes: &[u8]) -> Result<Vec<SnapshotRecord>, SnapshotError> {
    let fields = nul_fields(bytes)?;
    let mut records = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        index += 1;
        match field.first().copied() {
            Some(b'1') => records.push(parse_ordinary(field)?),
            Some(b'2') => {
                let original = fields.get(index).ok_or_else(|| {
                    SnapshotError::Malformed("rename record is missing original path".to_string())
                })?;
                index += 1;
                records.push(parse_rename(field, original)?);
            }
            Some(b'u') => records.push(parse_unmerged(field)?),
            Some(b'?') => records.push(simple_record(
                "?",
                path_after_prefix(field, 2)?,
                false,
                true,
            )?),
            Some(b'!') => {}
            _ => {
                return Err(SnapshotError::Malformed(format!(
                    "unknown porcelain-v2 record type: {:?}",
                    field.first()
                )));
            }
        }
    }
    Ok(records)
}

fn parse_ordinary(field: &[u8]) -> Result<SnapshotRecord, SnapshotError> {
    let parts = splitn_spaces(field, 9);
    if parts.len() != 9 || parts[0] != b"1" {
        return Err(SnapshotError::Malformed(
            "ordinary status record".to_string(),
        ));
    }
    record_from_xy(parts[1], parts[2], parts[8], None)
}

fn parse_rename(field: &[u8], original: &[u8]) -> Result<SnapshotRecord, SnapshotError> {
    let parts = splitn_spaces(field, 10);
    if parts.len() != 10 || parts[0] != b"2" {
        return Err(SnapshotError::Malformed("rename status record".to_string()));
    }
    if parts[8].first() == Some(&b'C') {
        return Err(SnapshotError::UnsupportedCopy);
    }
    if parts[8].first() != Some(&b'R') {
        return Err(SnapshotError::Malformed(
            "type-2 record is not a rename".to_string(),
        ));
    }
    record_from_xy(parts[1], parts[2], parts[9], Some(original))
}

fn parse_unmerged(field: &[u8]) -> Result<SnapshotRecord, SnapshotError> {
    let parts = splitn_spaces(field, 11);
    if parts.len() != 11 || parts[0] != b"u" {
        return Err(SnapshotError::Malformed(
            "unmerged status record".to_string(),
        ));
    }
    record_from_xy(parts[1], parts[2], parts[10], None)
}

fn record_from_xy(
    xy: &[u8],
    submodule: &[u8],
    path: &[u8],
    original: Option<&[u8]>,
) -> Result<SnapshotRecord, SnapshotError> {
    if xy.len() != 2 {
        return Err(SnapshotError::Malformed("status XY field".to_string()));
    }
    let status = String::from_utf8(xy.to_vec())
        .map_err(|_| SnapshotError::Malformed("non-ASCII status".to_string()))?;
    let path = checked_path(path)?;
    let original_path = original.map(checked_path).transpose()?;
    Ok(SnapshotRecord {
        status,
        path,
        original_path,
        staged: xy[0] != b'.',
        unstaged: xy[1] != b'.',
        submodule_state: (submodule != b"N...")
            .then(|| String::from_utf8_lossy(submodule).into_owned()),
    })
}

fn simple_record(
    status: &str,
    path: &[u8],
    staged: bool,
    unstaged: bool,
) -> Result<SnapshotRecord, SnapshotError> {
    Ok(SnapshotRecord {
        status: status.to_string(),
        path: checked_path(path)?,
        original_path: None,
        staged,
        unstaged,
        submodule_state: None,
    })
}

fn parse_name_status(bytes: &[u8]) -> Result<Vec<SnapshotRecord>, SnapshotError> {
    let fields = nul_fields(bytes)?;
    let mut records = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let status = fields[index];
        index += 1;
        if status.first() == Some(&b'C') {
            return Err(SnapshotError::UnsupportedCopy);
        }
        if status.first() == Some(&b'R') {
            let old = fields.get(index).ok_or_else(|| {
                SnapshotError::Malformed("diff rename is missing old path".to_string())
            })?;
            let new = fields.get(index + 1).ok_or_else(|| {
                SnapshotError::Malformed("diff rename is missing new path".to_string())
            })?;
            index += 2;
            records.push(SnapshotRecord {
                status: String::from_utf8(status.to_vec()).map_err(|_| {
                    SnapshotError::Malformed("diff status is not ASCII".to_string())
                })?,
                path: checked_path(new)?,
                original_path: Some(checked_path(old)?),
                staged: true,
                unstaged: false,
                submodule_state: None,
            });
            continue;
        }
        let path = fields
            .get(index)
            .ok_or_else(|| SnapshotError::Malformed("diff status is missing path".to_string()))?;
        index += 1;
        records.push(simple_record(
            std::str::from_utf8(status)
                .map_err(|_| SnapshotError::Malformed("diff status is not ASCII".to_string()))?,
            path,
            true,
            false,
        )?);
    }
    Ok(records)
}

fn checked_path(bytes: &[u8]) -> Result<RawPath, SnapshotError> {
    let path = RawPath::new(bytes);
    path.validate_repository_relative()
        .map_err(SnapshotError::InvalidPath)?;
    Ok(path)
}

fn nul_fields(bytes: &[u8]) -> Result<Vec<&[u8]>, SnapshotError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.last() != Some(&0) {
        return Err(SnapshotError::Malformed(
            "NUL-delimited output has no final terminator".to_string(),
        ));
    }
    let mut fields = bytes[..bytes.len() - 1]
        .split(|byte| *byte == 0)
        .collect::<Vec<_>>();
    if fields.iter().any(|field| field.is_empty()) {
        return Err(SnapshotError::Malformed("empty NUL field".to_string()));
    }
    fields.shrink_to_fit();
    Ok(fields)
}

fn path_after_prefix(field: &[u8], prefix: usize) -> Result<&[u8], SnapshotError> {
    field
        .get(prefix..)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| SnapshotError::Malformed("record is missing path".to_string()))
}

fn splitn_spaces(bytes: &[u8], count: usize) -> Vec<&[u8]> {
    bytes.splitn(count, |byte| *byte == b' ').collect()
}

fn sort_records(records: &mut [SnapshotRecord]) {
    records.sort_by(|left, right| {
        left.status
            .as_bytes()
            .cmp(right.status.as_bytes())
            .then_with(|| left.path.as_bytes().cmp(right.path.as_bytes()))
            .then_with(|| {
                left.original_path
                    .as_ref()
                    .map(RawPath::as_bytes)
                    .unwrap_or_default()
                    .cmp(
                        right
                            .original_path
                            .as_ref()
                            .map(RawPath::as_bytes)
                            .unwrap_or_default(),
                    )
            })
    });
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
