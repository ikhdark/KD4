use crate::model::RawPath;
use crate::model::RepositorySnapshot;
use crate::model::SnapshotRecord;
use crate::model::SnapshotSource;
#[cfg(unix)]
use std::ffi::OsString;
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
        let repository_root = canonical_path(repository_root)?;
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
        let repository_root = canonical_git_root(repository_root)?;
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
        let repository_root = canonical_git_root(repository_root)?;
        let base = resolve_commit(&repository_root, base)?;
        let head = resolve_commit(&repository_root, head)?;
        let (comparison_base, merge_base) = match mode {
            CommitComparisonMode::Direct => (base.clone(), None),
            CommitComparisonMode::PullRequestMergeBase => {
                let resolved = resolve_merge_base(&repository_root, &base, &head)?;
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
                base: Some(base),
                head: Some(head),
                merge_base,
                pull_request: mode == CommitComparisonMode::PullRequestMergeBase,
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

    pub fn commit_diff_fallback(
        repository_root: &Path,
        base: &str,
        head: &str,
        mode: CommitComparisonMode,
        reason: impl Into<String>,
    ) -> Self {
        let repository_root = canonical_git_root(repository_root).ok();
        let resolved_base = repository_root
            .as_deref()
            .and_then(|root| resolve_commit(root, base).ok());
        let resolved_head = repository_root
            .as_deref()
            .and_then(|root| resolve_commit(root, head).ok());
        let merge_base = match (
            mode,
            repository_root.as_deref(),
            resolved_base.as_deref(),
            resolved_head.as_deref(),
        ) {
            (CommitComparisonMode::PullRequestMergeBase, Some(root), Some(base), Some(head)) => {
                resolve_merge_base(root, base, head).ok()
            }
            _ => None,
        };
        Self {
            repository_root,
            source: SnapshotSource::CommitDiff {
                base: resolved_base,
                head: resolved_head,
                merge_base,
                pull_request: mode == CommitComparisonMode::PullRequestMergeBase,
            },
            records: Vec::new(),
            complete: false,
            fallback_reasons: vec![reason.into()],
        }
    }
}

fn canonical_path(path: &Path) -> Result<PathBuf, SnapshotError> {
    fs::canonicalize(path).map_err(|source| SnapshotError::Canonicalize {
        path: path.to_path_buf(),
        source,
    })
}

fn canonical_git_root(path: &Path) -> Result<PathBuf, SnapshotError> {
    let start = canonical_path(path)?;
    let output = run_git(
        &start,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
    )?;
    let bytes = single_line(&output, "Git top-level path")?;
    canonical_path(&path_from_git_bytes(bytes)?)
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
    let resolved_output = std::str::from_utf8(&output)
        .map_err(|_| SnapshotError::InvalidCommitObject(value.to_string()))?;
    let lines = resolved_output
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1
        || !(40..=64).contains(&lines[0].len())
        || !lines[0].as_bytes().iter().all(u8::is_ascii_hexdigit)
    {
        return Err(SnapshotError::InvalidCommitObject(value.to_string()));
    }
    Ok(lines[0].to_ascii_lowercase())
}

fn resolve_merge_base(
    repository_root: &Path,
    base: &str,
    head: &str,
) -> Result<String, SnapshotError> {
    let output = run_git(repository_root, &["merge-base", "--all", base, head])?;
    let base = parse_single_merge_base(&output)?;
    resolve_commit(repository_root, base)
}

fn parse_single_merge_base(output: &[u8]) -> Result<&str, SnapshotError> {
    let output = std::str::from_utf8(&output)
        .map_err(|_| SnapshotError::Malformed("merge-base output is not ASCII".to_string()))?;
    let bases = output
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if bases.len() != 1 {
        return Err(SnapshotError::AmbiguousMergeBase { count: bases.len() });
    }
    Ok(bases[0])
}

fn single_line<'a>(bytes: &'a [u8], label: &str) -> Result<&'a [u8], SnapshotError> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    if bytes.is_empty() || bytes.contains(&b'\n') || bytes.contains(&b'\r') || bytes.contains(&0) {
        return Err(SnapshotError::Malformed(format!(
            "{label} is not exactly one line"
        )));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf, SnapshotError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(windows)]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf, SnapshotError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| SnapshotError::Malformed("Git top-level path is not UTF-8".to_string()))?;
    Ok(PathBuf::from(text))
}

#[cfg(not(any(unix, windows)))]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf, SnapshotError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| SnapshotError::Malformed("Git top-level path is not UTF-8".to_string()))?;
    Ok(PathBuf::from(text))
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
        .args(["-c", "status.relativePaths=false"])
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
            Some(b'?') if field.starts_with(b"? ") => records.push(simple_record(
                "?",
                path_after_prefix(field, 2)?,
                false,
                true,
            )?),
            Some(b'!') => {
                return Err(SnapshotError::Malformed(
                    "ignored record was not requested".to_string(),
                ));
            }
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
    validate_submodule(parts[2])?;
    validate_modes_and_oids(&parts, 3..=5, 6..=7)?;
    record_from_xy(parts[1], parts[2], parts[8], None, false)
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
    validate_rename_status(parts[8])?;
    validate_submodule(parts[2])?;
    validate_modes_and_oids(&parts, 3..=5, 6..=7)?;
    record_from_xy(parts[1], parts[2], parts[9], Some(original), false)
}

fn parse_unmerged(field: &[u8]) -> Result<SnapshotRecord, SnapshotError> {
    let parts = splitn_spaces(field, 11);
    if parts.len() != 11 || parts[0] != b"u" {
        return Err(SnapshotError::Malformed(
            "unmerged status record".to_string(),
        ));
    }
    validate_submodule(parts[2])?;
    validate_modes_and_oids(&parts, 3..=6, 7..=9)?;
    record_from_xy(parts[1], parts[2], parts[10], None, true)
}

fn record_from_xy(
    xy: &[u8],
    submodule: &[u8],
    path: &[u8],
    original: Option<&[u8]>,
    unmerged: bool,
) -> Result<SnapshotRecord, SnapshotError> {
    if xy.len() != 2 {
        return Err(SnapshotError::Malformed("status XY field".to_string()));
    }
    let valid = if unmerged {
        matches!(xy, b"DD" | b"AU" | b"UD" | b"UA" | b"DU" | b"AA" | b"UU")
    } else {
        xy != b".."
            && xy
                .iter()
                .all(|status| matches!(status, b'.' | b'M' | b'A' | b'D' | b'R' | b'T'))
    };
    if !valid {
        return Err(SnapshotError::Malformed(
            "unsupported status XY field".to_string(),
        ));
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

fn validate_submodule(field: &[u8]) -> Result<(), SnapshotError> {
    let valid = field == b"N..."
        || (field.len() == 4
            && field[0] == b'S'
            && matches!(field[1], b'.' | b'C')
            && matches!(field[2], b'.' | b'M')
            && matches!(field[3], b'.' | b'U'));
    if valid {
        Ok(())
    } else {
        Err(SnapshotError::Malformed(
            "invalid submodule field".to_string(),
        ))
    }
}

fn validate_modes_and_oids(
    parts: &[&[u8]],
    modes: std::ops::RangeInclusive<usize>,
    oids: std::ops::RangeInclusive<usize>,
) -> Result<(), SnapshotError> {
    if modes.clone().any(|index| {
        parts[index].len() != 6 || !parts[index].iter().all(|byte| matches!(byte, b'0'..=b'7'))
    }) {
        return Err(SnapshotError::Malformed("invalid Git mode".to_string()));
    }
    if oids.clone().any(|index| {
        !matches!(parts[index].len(), 40 | 64) || !parts[index].iter().all(u8::is_ascii_hexdigit)
    }) {
        return Err(SnapshotError::Malformed(
            "invalid Git object ID".to_string(),
        ));
    }
    Ok(())
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
            validate_rename_status(status)?;
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
        if !matches!(status, b"A" | b"D" | b"M" | b"T") {
            return Err(SnapshotError::Malformed(format!(
                "unsupported diff status: {}",
                String::from_utf8_lossy(status)
            )));
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

fn validate_rename_status(status: &[u8]) -> Result<(), SnapshotError> {
    let score = status
        .strip_prefix(b"R")
        .filter(|score| score.len() == 3 && score.iter().all(u8::is_ascii_digit))
        .ok_or_else(|| SnapshotError::Malformed("invalid rename score".to_string()))?;
    let score = std::str::from_utf8(score)
        .ok()
        .and_then(|score| score.parse::<u16>().ok())
        .ok_or_else(|| SnapshotError::Malformed("invalid rename score".to_string()))?;
    if score > 100 {
        return Err(SnapshotError::Malformed(
            "rename score exceeds 100".to_string(),
        ));
    }
    Ok(())
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
