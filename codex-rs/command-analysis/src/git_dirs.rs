use std::path::PathBuf;

use codex_utils_absolute_path::AbsolutePathBuf;

/// Resolves a repository's git directory, common directory (worktrees) and
/// symbolic HEAD target without running git.
pub fn resolve_git_dirs(
    repo_root: &AbsolutePathBuf,
) -> Option<(PathBuf, PathBuf, Option<PathBuf>)> {
    let (git_dir, common_dir) = resolve_git_directories(repo_root)?;
    let head_ref = std::fs::read_to_string(git_dir.join("HEAD"))
        .ok()
        .and_then(|head| {
            head.trim()
                .strip_prefix("ref:")
                .map(str::trim)
                .map(PathBuf::from)
        });
    Some((git_dir, common_dir, head_ref))
}

/// Resolve only directory identity when the caller reads HEAD itself.
pub fn resolve_git_directories(repo_root: &AbsolutePathBuf) -> Option<(PathBuf, PathBuf)> {
    git_directories_at(repo_root.as_path()).ok().flatten()
}

/// Distinguish missing repository metadata from an observation that failed.
pub(crate) fn git_directories_at(
    repo_root: &std::path::Path,
) -> std::io::Result<Option<(PathBuf, PathBuf)>> {
    use std::io::{self, ErrorKind};

    let marker = repo_root.join(".git");
    let git_dir = match std::fs::metadata(&marker) {
        Ok(metadata) if metadata.is_dir() => marker,
        Ok(_) => {
            let pointer = std::fs::read_to_string(marker)?;
            let target = pointer.trim().strip_prefix("gitdir:").map(str::trim)
                .filter(|target| !target.is_empty())
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "invalid .git directory pointer"))?;
            repo_root.join(target)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let common_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(path) if !path.trim().is_empty() => git_dir.join(path.trim()),
        Ok(_) => return Err(io::Error::new(ErrorKind::InvalidData, "empty Git common directory")),
        Err(error) if error.kind() == ErrorKind::NotFound => git_dir.clone(),
        Err(error) => return Err(error),
    };
    Ok(Some((git_dir, common_dir)))
}
