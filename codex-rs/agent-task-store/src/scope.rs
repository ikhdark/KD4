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
    let repository_identity_input = git_common_directory(&canonical_root)
        .and_then(|path| std::fs::canonicalize(path).ok())
        .map(|path| filesystem_identity_bytes(&path))
        .unwrap_or_else(|| workspace_identity_input.clone());
    Ok(RepositoryIdentity {
        id: format!("{:x}", Sha256::digest(&repository_identity_input)),
        workspace_id: format!("{:x}", Sha256::digest(&workspace_identity_input)),
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

pub(crate) fn path_comparison_key(path: &str) -> String {
    comparison_key(path)
}

pub(crate) fn filesystem_paths_equal(left: &str, right: &str) -> bool {
    paths_equal(left, right)
}

pub(crate) fn relative_path_identity(path: &Path) -> String {
    if let Some(path) = path.to_str()
        && !path.starts_with(ENCODED_PATH_PREFIX)
    {
        return path.replace(std::path::MAIN_SEPARATOR, "/");
    }
    format!(
        "{ENCODED_PATH_PREFIX}{}",
        hex_encode(&native_os_bytes(path.as_os_str()))
    )
}

pub(crate) fn absolute_repo_path(repo_root: &Path, relative: &str) -> PathBuf {
    if let Some(encoded) = relative.strip_prefix(ENCODED_PATH_PREFIX)
        && let Some(path) = native_os_string_from_hex(encoded)
    {
        return repo_root.join(PathBuf::from(path));
    }
    repo_root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
}

const ENCODED_PATH_PREFIX: &str = ":native-path:";

fn filesystem_identity_bytes(path: &Path) -> Vec<u8> {
    let bytes = native_os_bytes(path.as_os_str());
    if cfg!(windows) {
        // Canonical Windows paths normally preserve the filesystem's spelling. Lowercase
        // valid Unicode paths for compatibility with the legacy identity while retaining a
        // lossless wide-character fallback for paths that cannot be represented as UTF-8.
        if let Some(path) = path.to_str() {
            return path.to_lowercase().into_bytes();
        }
    } else if let Some(path) = path.to_str() {
        return path.as_bytes().to_vec();
    }
    let mut identity = if cfg!(windows) {
        b"windows\0".to_vec()
    } else {
        b"unix\0".to_vec()
    };
    identity.extend(bytes);
    identity
}

#[cfg(unix)]
fn native_os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn native_os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>()
}

#[cfg(unix)]
fn native_os_string_from_hex(value: &str) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt;
    Some(std::ffi::OsString::from_vec(hex_decode(value)?))
}

#[cfg(windows)]
fn native_os_string_from_hex(value: &str) -> Option<std::ffi::OsString> {
    use std::os::windows::ffi::OsStringExt;
    let bytes = hex_decode(value)?;
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    Some(std::ffi::OsString::from_wide(
        &bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>(),
    ))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0xf) as usize] as char);
    }
    encoded
}

fn hex_decode(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    #[test]
    fn audit_workspace_case_identity_is_platform_aware() {
        if cfg!(windows) {
            assert_eq!(comparison_key("Src/Lib.rs"), comparison_key("src/lib.rs"));
        } else {
            assert_ne!(comparison_key("Src/Lib.rs"), comparison_key("src/lib.rs"));
        }
    }

    #[test]
    fn audit_workspace_native_path_encoding_is_lossless() {
        #[cfg(unix)]
        let (left, right) = {
            use std::os::unix::ffi::OsStringExt;
            (
                PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0x80])),
                PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0x81])),
            )
        };
        #[cfg(windows)]
        let (left, right) = {
            use std::os::windows::ffi::OsStringExt;
            (
                PathBuf::from(std::ffi::OsString::from_wide(&[b'a' as u16, 0xd800])),
                PathBuf::from(std::ffi::OsString::from_wide(&[b'a' as u16, 0xd801])),
            )
        };

        let left_identity = relative_path_identity(&left);
        let right_identity = relative_path_identity(&right);
        assert_ne!(left_identity, right_identity);
        let root = Path::new("root");
        assert_eq!(absolute_repo_path(root, &left_identity), root.join(left));
        assert_eq!(absolute_repo_path(root, &right_identity), root.join(right));
    }
}
