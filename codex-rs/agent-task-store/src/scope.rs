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
    let workspace_identity_input = if cfg!(windows) {
        canonical_path.to_lowercase()
    } else {
        canonical_path.clone()
    };
    let repository_identity_input = git_common_directory(&canonical_root)
        .and_then(|path| std::fs::canonicalize(path).ok())
        .map(|path| path.to_string_lossy().into_owned())
        .map(|path| {
            if cfg!(windows) {
                path.to_lowercase()
            } else {
                path
            }
        })
        .unwrap_or_else(|| workspace_identity_input.clone());
    Ok(RepositoryIdentity {
        id: format!("{:x}", Sha256::digest(repository_identity_input.as_bytes())),
        workspace_id: format!("{:x}", Sha256::digest(workspace_identity_input.as_bytes())),
        canonical_root,
        canonical_path,
    })
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

fn git_common_directory(canonical_root: &Path) -> Option<PathBuf> {
    let dot_git = canonical_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let marker = std::fs::read_to_string(&dot_git).ok()?;
    let git_dir = marker
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))?
        .trim();
    let git_dir = Path::new(git_dir);
    let git_dir = if git_dir.is_absolute() {
        git_dir.to_path_buf()
    } else {
        canonical_root.join(git_dir)
    };
    let common = std::fs::read_to_string(git_dir.join("commondir")).ok();
    match common {
        Some(common) => {
            let common = Path::new(common.trim());
            Some(if common.is_absolute() {
                common.to_path_buf()
            } else {
                git_dir.join(common)
            })
        }
        None => Some(git_dir),
    }
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

    pub(crate) fn covers_scope(&self, other: &Self) -> bool {
        paths_equal(&self.path, &other.path) && (self.recursive || !other.recursive)
            || self.recursive && is_descendant(&self.path, &other.path)
    }
}

pub fn normalize_repo_scopes(
    repo_root: &Path,
    scopes: &[RepoScope],
) -> StoreResult<Vec<RepoScope>> {
    let canonical_root = repository_identity(repo_root)?.canonical_root;
    let mut normalized = Vec::with_capacity(scopes.len());
    let mut seen = HashSet::with_capacity(scopes.len());

    for scope in scopes {
        let path = normalize_lexically(&scope.path)?;
        let path = canonical_relative_identity(&canonical_root, &path)?;
        let duplicate_key = if cfg!(windows) {
            path.to_lowercase()
        } else {
            path.clone()
        };
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
    let canonical_root = repository_identity(repo_root)?.canonical_root;
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
        path.to_string()
    }
}

pub(crate) fn absolute_repo_path(repo_root: &Path, relative: &str) -> PathBuf {
    repo_root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
}
