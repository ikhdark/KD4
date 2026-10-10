use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashSet;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use crate::StoreError;
use crate::StoreResult;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepositoryIdentity {
    pub id: String,
    pub workspace_id: String,
    pub canonical_root: PathBuf,
    pub canonical_path: String,
}

pub(crate) fn repository_identity(repo_root: &Path) -> StoreResult<RepositoryIdentity> {
    let canonical_root = std::fs::canonicalize(repo_root).map_err(|error| {
        StoreError::InvalidScope(format!(
            "repository root {} cannot be canonicalized: {error}",
            repo_root.display()
        ))
    })?;
    let canonical_path = canonical_root.to_string_lossy().into_owned();
    let workspace_identity_input = filesystem_identity_bytes(&canonical_root);
    let repository_identity_input = match git_common_directory(&canonical_root)? {
        Some(path) => {
            let path = std::fs::canonicalize(&path).map_err(|error| {
                StoreError::InvalidScope(format!(
                    "Git directory {} cannot be canonicalized: {error}",
                    path.display()
                ))
            })?;
            filesystem_identity_bytes(&path)
        }
        None => workspace_identity_input.clone(),
    };
    Ok(RepositoryIdentity {
        id: format!("{:x}", Sha256::digest(&repository_identity_input)),
        workspace_id: format!("{:x}", Sha256::digest(&workspace_identity_input)),
        canonical_root,
        canonical_path,
    })
}

pub(crate) async fn repository_identity_async(repo_root: &Path) -> StoreResult<RepositoryIdentity> {
    let repo_root = repo_root.to_path_buf();
    tokio::task::spawn_blocking(move || repository_identity(&repo_root))
        .await
        .map_err(|error| StoreError::CorruptData(format!("repository task failed: {error}")))?
}

pub(crate) async fn normalize_repo_path_async(repo_root: &Path, path: &str) -> StoreResult<String> {
    let repo_root = repo_root.to_path_buf();
    let path = path.to_string();
    tokio::task::spawn_blocking(move || normalize_repo_path(&repo_root, &path))
        .await
        .map_err(|error| {
            StoreError::CorruptData(format!("path normalization task failed: {error}"))
        })?
}

/// Stable repository-lineage identity shared by the coordination store and its callers.
///
/// Linked worktrees resolve to the same lineage through Git's common directory, while
/// non-Git directories fall back to their canonical filesystem identity.
pub fn repository_lineage_id(repo_root: &Path) -> StoreResult<String> {
    Ok(repository_identity(repo_root)?.id)
}

/// Stable identity for one concrete checkout or linked worktree.
pub fn repository_workspace_id(repo_root: &Path) -> StoreResult<String> {
    Ok(repository_identity(repo_root)?.workspace_id)
}

fn git_common_directory(canonical_root: &Path) -> StoreResult<Option<PathBuf>> {
    let dot_git = canonical_root.join(".git");
    match std::fs::symlink_metadata(&dot_git) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(StoreError::InvalidScope(format!(
                "Git metadata {} cannot be inspected: {error}",
                dot_git.display()
            )));
        }
    }
    if dot_git.is_dir() {
        return Ok(Some(dot_git));
    }
    let marker = std::fs::read_to_string(&dot_git).map_err(|error| {
        StoreError::InvalidScope(format!(
            "Git metadata {} cannot be read: {error}",
            dot_git.display()
        ))
    })?;
    let git_dir = marker
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            StoreError::InvalidScope(format!(
                "Git metadata {} has no nonempty gitdir",
                dot_git.display()
            ))
        })?;
    let git_dir = Path::new(git_dir);
    let git_dir = if git_dir.is_absolute() {
        git_dir.to_path_buf()
    } else {
        canonical_root.join(git_dir)
    };
    let common_path = git_dir.join("commondir");
    match std::fs::symlink_metadata(&common_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(git_dir));
        }
        Err(error) => {
            return Err(StoreError::InvalidScope(format!(
                "Git metadata {} cannot be inspected: {error}",
                common_path.display()
            )));
        }
    }
    let common = std::fs::read_to_string(&common_path).map_err(|error| {
        StoreError::InvalidScope(format!(
            "Git metadata {} cannot be read: {error}",
            common_path.display()
        ))
    })?;
    let common = common.trim();
    if common.is_empty() {
        return Err(StoreError::InvalidScope(format!(
            "Git metadata {} has an empty commondir",
            common_path.display()
        )));
    }
    let common = Path::new(common);
    Ok(Some(if common.is_absolute() {
        common.to_path_buf()
    } else {
        git_dir.join(common)
    }))
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct RepoScope {
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
}

impl RepoScope {
    pub fn covers_path(&self, path: &str) -> bool {
        paths_equal(&self.path, path) || self.recursive && is_descendant(&self.path, path)
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        paths_equal(&self.path, &other.path)
            || self.recursive && is_descendant(&self.path, &other.path)
            || other.recursive && is_descendant(&other.path, &self.path)
    }

}

pub fn normalize_repo_scopes(
    repo_root: &Path,
    scopes: &[RepoScope],
) -> StoreResult<Vec<RepoScope>> {
    let canonical_root = std::fs::canonicalize(repo_root)?;
    let mut normalized = Vec::with_capacity(scopes.len());
    let mut seen = HashSet::with_capacity(scopes.len());

    for scope in scopes {
        let path = normalize_lexically(&scope.path)?;
        let path = canonical_relative_identity(&canonical_root, &path)?;
        let duplicate_key = comparison_key(&path);
        if !seen.insert(duplicate_key) {
            return Err(StoreError::InvalidScope(format!(
                "duplicate scope path {path}"
            )));
        }
        normalized.push(RepoScope {
            path,
            recursive: scope.recursive,
        });
    }

    Ok(normalized)
}

pub fn normalize_repo_path(repo_root: &Path, path: &str) -> StoreResult<String> {
    let canonical_root = std::fs::canonicalize(repo_root)?;
    let normalized = normalize_lexically(path)?;
    canonical_relative_identity(&canonical_root, &normalized)
}


fn normalize_lexically(path: &str) -> StoreResult<String> {
    if path.trim().is_empty() {
        return Err(StoreError::InvalidScope(
            "scope path cannot be empty".to_string(),
        ));
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(StoreError::InvalidScope(format!(
            "absolute scope path is not allowed: {path}"
        )));
    }

    let mut components = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(value) => components.push(value.to_string_lossy().into_owned()),
            Component::ParentDir => {
                return Err(StoreError::InvalidScope(format!(
                    "scope traversal is not allowed: {path}"
                )));
            }
            Component::CurDir if path.trim() == "." => {}
            Component::CurDir => {
                return Err(StoreError::InvalidScope(format!(
                    "scope dot components are not allowed: {path}"
                )));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(StoreError::InvalidScope(format!(
                    "absolute scope path is not allowed: {path}"
                )));
            }
        }
    }
    if components.is_empty() && path.trim() == "." {
        return Ok(".".to_string());
    }
    if components.is_empty() {
        return Err(StoreError::InvalidScope(
            "scope path cannot be empty".to_string(),
        ));
    }
    Ok(components.join("/"))
}

fn canonical_relative_identity(canonical_root: &Path, relative: &str) -> StoreResult<String> {
    let target = canonical_root.join(relative);
    let mut existing = target.as_path();
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| {
            StoreError::InvalidScope(format!("scope has no existing ancestor: {relative}"))
        })?;
    }
    let canonical_existing = std::fs::canonicalize(existing).map_err(|error| {
        StoreError::InvalidScope(format!(
            "scope ancestor {} cannot be canonicalized: {error}",
            existing.display()
        ))
    })?;
    if !canonical_existing.starts_with(canonical_root) {
        return Err(StoreError::InvalidScope(format!(
            "scope resolves outside the repository through a symlink: {relative}"
        )));
    }
    let suffix = target.strip_prefix(existing).map_err(|_| {
        StoreError::InvalidScope(format!(
            "scope cannot be made repository-relative: {relative}"
        ))
    })?;
    let canonical_target = canonical_existing.join(suffix);
    let canonical_relative = canonical_target.strip_prefix(canonical_root).map_err(|_| {
        StoreError::InvalidScope(format!("scope resolves outside the repository: {relative}"))
    })?;
    let components = canonical_relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value.to_string_lossy().into_owned()),
            _ => Err(StoreError::InvalidScope(format!(
                "scope has an invalid canonical identity: {relative}"
            ))),
        })
        .collect::<StoreResult<Vec<_>>>()?;
    if components.is_empty() && relative == "." {
        return Ok(".".to_string());
    }
    if components.is_empty() {
        return Err(StoreError::InvalidScope(
            "scope path cannot resolve to the repository root".to_string(),
        ));
    }
    Ok(components.join("/"))
}

fn is_descendant(parent: &str, child: &str) -> bool {
    let parent = comparison_key(parent);
    let child = comparison_key(child);
    if parent == "." {
        return child != ".";
    }
    child
        .strip_prefix(&parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn paths_equal(left: &str, right: &str) -> bool {
    comparison_key(left) == comparison_key(right)
}

fn comparison_key(path: &str) -> String {
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_owned()
    }
}


pub(crate) fn filesystem_paths_equal(left: &str, right: &str) -> bool {
    paths_equal(left, right)
}




fn filesystem_identity_bytes(path: &Path) -> Vec<u8> {
    let bytes = native_os_bytes(path.as_os_str());
    // Canonical Windows paths normally preserve the filesystem's spelling. Lowercase
    // valid Unicode paths for compatibility with the legacy identity while retaining a
    // lossless wide-character fallback for paths that cannot be represented as UTF-8.
    if let Some(path) = path.to_str() {
        return path.to_lowercase().into_bytes();
    }
    let mut identity = b"windows\0".to_vec();
    identity.extend(bytes);
    identity
}

fn native_os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>()
}




#[cfg(test)]
mod audit_tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn audit_workspace_case_identity_is_windows_case_insensitive() {
        assert_eq!(comparison_key("Src/Lib.rs"), comparison_key("src/lib.rs"));
        let scope = RepoScope {
            path: "Src".to_string(),
            recursive: true,
        };
        assert!(scope.covers_path("src/lib.rs"));
        // Case folding must not merge distinct names.
        assert!(!scope.covers_path("srcs/lib.rs"));
    }

    #[test]
    #[cfg(windows)]
    fn repository_native_identity_is_lossless() {
        let (left, right) = {
            use std::os::windows::ffi::OsStringExt;
            (
                PathBuf::from(std::ffi::OsString::from_wide(&[b'a' as u16, 0xd800])),
                PathBuf::from(std::ffi::OsString::from_wide(&[b'a' as u16, 0xd801])),
            )
        };

        let left_identity = filesystem_identity_bytes(&left);
        let right_identity = filesystem_identity_bytes(&right);
        assert_ne!(left_identity, right_identity);
    }
}
