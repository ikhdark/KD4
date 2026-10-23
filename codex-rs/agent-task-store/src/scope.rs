use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
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
        id: hex::encode(Sha256::digest(&repository_identity_input)),
        workspace_id: hex::encode(Sha256::digest(&workspace_identity_input)),
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

    for scope in scopes {
        let path = normalize_lexically(&scope.path)?;
        let path = canonical_relative_identity(&canonical_root, &path)?;
        normalized.push(RepoScope {
            path,
            recursive: scope.recursive,
        });
    }
    let paths = normalized
        .iter()
        .map(|scope| scope.path.clone())
        .collect::<Vec<_>>();
    let mut seen = HashSet::with_capacity(paths.len());
    for (scope, key) in normalized
        .iter()
        .zip(repo_path_identity_keys(&canonical_root, &paths)?)
    {
        if !seen.insert(key) {
            return Err(StoreError::InvalidScope(format!(
                "duplicate scope path {}",
                scope.path
            )));
        }
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
    // Normalization resolves existing aliases to canonical filesystem spelling.
    // Windows alone is not proof that a directory is case-insensitive.
    path.to_owned()
}

pub(crate) fn filesystem_paths_equal(left: &str, right: &str) -> bool {
    paths_equal(left, right)
}

/// Whether lookups in a directory resolve spellings that differ only by case
/// to the same entry. Observed from the filesystem rather than the platform:
/// Windows enables case sensitivity per directory.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ScopeCaseSemantics {
    /// Lookups distinguish case, or the semantics could not be observed.
    #[default]
    Exact,
    IgnoresCase,
}

/// Case semantics observed for an admission candidate's scopes before its
/// transaction opens, so admission never holds the database while it waits
/// on the filesystem.
#[derive(Clone, Debug, Default)]
pub(crate) struct CandidateScopeCase {
    read: Vec<ScopeCaseSemantics>,
    write: Vec<ScopeCaseSemantics>,
}

impl CandidateScopeCase {
    pub(crate) fn observe(
        repo_root: &Path,
        read_scope: &[RepoScope],
        write_scope: &[RepoScope],
    ) -> Self {
        let Ok(mut observer) = CaseObserver::new(repo_root) else {
            // Unobservable semantics keep exact comparison.
            return Self::default();
        };
        Self {
            read: observer.scopes(read_scope),
            write: observer.scopes(write_scope),
        }
    }

    pub(crate) fn read(&self, index: usize) -> ScopeCaseSemantics {
        self.read.get(index).copied().unwrap_or_default()
    }

    pub(crate) fn write(&self, index: usize) -> ScopeCaseSemantics {
        self.write.get(index).copied().unwrap_or_default()
    }
}

/// Overlap between a persisted scope and an admission candidate under the
/// case semantics observed for the candidate. Normalization keeps the declared
/// spelling of components that did not exist yet, so a persisted scope can
/// differ from a candidate alias by case alone. Aliases share directories, so
/// the candidate's observation governs both spellings.
pub(crate) fn scopes_overlap_observed(
    existing: &RepoScope,
    candidate: &RepoScope,
    semantics: ScopeCaseSemantics,
) -> bool {
    if existing.overlaps(candidate) {
        return true;
    }
    if semantics != ScopeCaseSemantics::IgnoresCase {
        return false;
    }
    let existing_key = case_folded(&existing.path);
    let candidate_key = case_folded(&candidate.path);
    existing_key == candidate_key
        || existing.recursive && is_descendant(&existing_key, &candidate_key)
        || candidate.recursive && is_descendant(&candidate_key, &existing_key)
}

/// Deduplication keys for normalized repository paths, in input order.
///
/// Normalization resolves existing aliases to the filesystem's spelling, so
/// only spellings that still differ by case alone can name one entry: a path
/// that does not exist yet, or one created after it was declared. Those keys
/// fold exactly where the deepest existing directory is observed to ignore
/// case. Keys compare paths; persist the normalized paths themselves.
pub fn repo_path_identity_keys(repo_root: &Path, paths: &[String]) -> StoreResult<Vec<String>> {
    let mut spellings = HashMap::<String, HashSet<&str>>::new();
    for path in paths {
        spellings
            .entry(case_folded(path))
            .or_default()
            .insert(path.as_str());
    }
    if spellings.values().all(|spellings| spellings.len() < 2) {
        return Ok(paths.to_vec());
    }
    let mut observer = CaseObserver::new(repo_root)?;
    Ok(paths
        .iter()
        .map(|path| {
            let folded = case_folded(path);
            let ambiguous = spellings
                .get(&folded)
                .is_some_and(|spellings| spellings.len() > 1);
            if ambiguous && observer.semantics(path) == ScopeCaseSemantics::IgnoresCase {
                // Paths never contain NUL, so a folded key cannot equal an exact one.
                format!("\0{folded}")
            } else {
                path.clone()
            }
        })
        .collect())
}

/// Entries read while looking for a name whose case can be swapped, bounded
/// so a very large directory cannot stall normalization or admission.
const CASE_PROBE_ENTRY_LIMIT: usize = 64;

/// Observes each directory's case semantics once per operation.
struct CaseObserver {
    canonical_root: PathBuf,
    observed: HashMap<PathBuf, ScopeCaseSemantics>,
}

impl CaseObserver {
    fn new(repo_root: &Path) -> StoreResult<Self> {
        Ok(Self {
            canonical_root: std::fs::canonicalize(repo_root)?,
            observed: HashMap::new(),
        })
    }

    fn scopes(&mut self, scopes: &[RepoScope]) -> Vec<ScopeCaseSemantics> {
        scopes
            .iter()
            .map(|scope| self.semantics(&scope.path))
            .collect()
    }

    /// Semantics of the deepest existing directory at or above a normalized
    /// path. Components that do not exist yet will be created under it.
    fn semantics(&mut self, normalized_path: &str) -> ScopeCaseSemantics {
        let Some(directory) = deepest_existing_directory(&self.canonical_root, normalized_path)
        else {
            return ScopeCaseSemantics::Exact;
        };
        *self
            .observed
            .entry(directory)
            .or_insert_with_key(|directory| observe_case_semantics(directory.as_path()))
    }
}

fn deepest_existing_directory(canonical_root: &Path, normalized_path: &str) -> Option<PathBuf> {
    let target = if normalized_path == "." {
        canonical_root.to_path_buf()
    } else {
        canonical_root.join(normalized_path)
    };
    let mut candidate = target.as_path();
    loop {
        if std::fs::metadata(candidate).is_ok_and(|metadata| metadata.is_dir()) {
            return Some(candidate.to_path_buf());
        }
        candidate = candidate.parent()?;
        if !candidate.starts_with(canonical_root) {
            return None;
        }
    }
}

fn observe_case_semantics(directory: &Path) -> ScopeCaseSemantics {
    let observed = std::fs::read_dir(directory).ok().and_then(|entries| {
        entries
            .take(CASE_PROBE_ENTRY_LIMIT)
            .filter_map(Result::ok)
            // A symlink's target can satisfy both spellings in a case-sensitive directory.
            .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_symlink()))
            .find_map(|entry| lookup_ignores_case(directory, &entry.file_name()))
    });
    // Without a usable entry, fall back to the directory's own name in its
    // parent: a new directory starts with its parent's semantics.
    let observed = observed.or_else(|| {
        let name = directory.file_name()?;
        lookup_ignores_case(directory.parent()?, name)
    });
    match observed {
        Some(true) => ScopeCaseSemantics::IgnoresCase,
        Some(false) | None => ScopeCaseSemantics::Exact,
    }
}

/// Resolve an existing entry through its case-swapped spelling. A miss, or a
/// distinct sibling under that spelling, proves the lookup is case-sensitive.
fn lookup_ignores_case(directory: &Path, name: &std::ffi::OsStr) -> Option<bool> {
    let name = name.to_str()?;
    let swapped = swap_ascii_case(name);
    if swapped == name {
        return None;
    }
    let swapped = directory.join(swapped);
    match std::fs::symlink_metadata(&swapped) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
        Ok(_) => {
            let original = std::fs::canonicalize(directory.join(name)).ok()?;
            let swapped = std::fs::canonicalize(&swapped).ok()?;
            Some(original == swapped)
        }
    }
}

fn swap_ascii_case(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else {
                character.to_ascii_uppercase()
            }
        })
        .collect()
}

/// One-to-one simple uppercase mapping of BMP characters, the per-UTF-16-unit
/// comparison case-insensitive filesystems apply. Unlike full Unicode folding
/// it never changes a name's length.
fn case_folded(path: &str) -> String {
    path.chars()
        .map(|character| {
            if u32::from(character) > 0xFFFF {
                return character;
            }
            let mut upper = character.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(single), None) => single,
                _ => character,
            }
        })
        .collect()
}

fn filesystem_identity_bytes(path: &Path) -> Vec<u8> {
    // A new domain keeps legacy folded identities from being treated as proof
    // of equality. The old store rows remain intact, not silently merged.
    let mut identity = b"windows-exact-v2\0".to_vec();
    identity.extend(native_os_bytes(path.as_os_str()));
    identity
}

/// Recognize old folded bindings only to leave them historical during upgrade.
/// This must not be used as current filesystem identity or scope authorization.
pub(crate) fn is_legacy_workspace_id(path: &Path, id: &str) -> bool {
    let bytes = if let Some(path) = path.to_str() {
        path.to_lowercase().into_bytes()
    } else {
        let mut bytes = b"windows\0".to_vec();
        bytes.extend(native_os_bytes(path.as_os_str()));
        bytes
    };
    hex::encode(Sha256::digest(bytes)) == id
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
    fn scope_identity_does_not_assume_windows_is_case_insensitive() {
        assert_ne!(comparison_key("Src/Lib.rs"), comparison_key("src/lib.rs"));
        let scope = RepoScope {
            path: "Src".to_string(),
            recursive: true,
        };
        assert!(scope.covers_path("Src/lib.rs"));
        assert!(!scope.covers_path("src/lib.rs"));
        assert_ne!(
            filesystem_identity_bytes(Path::new(r"C:\Repo")),
            filesystem_identity_bytes(Path::new(r"C:\repo"))
        );
        let legacy = hex::encode(Sha256::digest(b"c:\\repo"));
        assert!(is_legacy_workspace_id(Path::new(r"C:\Repo"), &legacy));
        assert!(!is_legacy_workspace_id(Path::new(r"C:\Repo"), "corrupt"));
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

    #[test]
    fn existing_scope_aliases_follow_canonical_filesystem_spelling() {
        let temp = tempfile::tempdir().unwrap();
        let upper = temp.path().join("Owner.rs");
        let lower = temp.path().join("owner.rs");
        std::fs::write(&upper, "upper").unwrap();
        std::fs::write(&lower, "lower").unwrap();
        let aliases =
            std::fs::canonicalize(&upper).unwrap() == std::fs::canonicalize(&lower).unwrap();
        let upper = normalize_repo_path(temp.path(), "Owner.rs").unwrap();
        let lower = normalize_repo_path(temp.path(), "owner.rs").unwrap();
        assert_eq!(upper == lower, aliases);
    }

    #[test]
    fn observed_case_semantics_fold_only_where_lookups_ignore_case() {
        let existing = RepoScope {
            path: "src/new.rs".to_string(),
            recursive: false,
        };
        let candidate = RepoScope {
            path: "src/New.rs".to_string(),
            recursive: false,
        };
        let directory = RepoScope {
            path: "Src".to_string(),
            recursive: true,
        };
        let sibling = RepoScope {
            path: "srcs/New.rs".to_string(),
            recursive: false,
        };
        // Case-sensitive negatives survive: exact semantics never fold.
        assert!(!scopes_overlap_observed(&existing, &candidate, ScopeCaseSemantics::Exact));
        assert!(!scopes_overlap_observed(&directory, &candidate, ScopeCaseSemantics::Exact));
        assert!(scopes_overlap_observed(&existing, &candidate, ScopeCaseSemantics::IgnoresCase));
        assert!(scopes_overlap_observed(&directory, &candidate, ScopeCaseSemantics::IgnoresCase));
        // Folding never merges distinct names, and keeps one-to-one mappings.
        assert!(!scopes_overlap_observed(&directory, &sibling, ScopeCaseSemantics::IgnoresCase));
        assert_eq!(case_folded("straße/ß.rs"), "STRAßE/ß.RS");
    }

    #[test]
    fn missing_case_aliases_follow_the_observed_directory() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src");
        std::fs::create_dir(&source).unwrap();
        // Independent oracle: exclusive creation of an alias collides exactly
        // when lookups in this directory ignore case.
        std::fs::write(source.join("Oracle.rs"), "oracle").unwrap();
        let ignores_case = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(source.join("oracle.RS"))
        {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => true,
            Err(error) => panic!("case alias oracle failed: {error}"),
        };

        let upper = normalize_repo_path(temp.path(), "src/New.rs").unwrap();
        let lower = normalize_repo_path(temp.path(), "src/new.rs").unwrap();
        assert_eq!((upper.as_str(), lower.as_str()), ("src/New.rs", "src/new.rs"));
        let candidate = RepoScope {
            path: upper.clone(),
            recursive: false,
        };
        let observed = CandidateScopeCase::observe(temp.path(), &[], &[candidate.clone()]);
        let expected = if ignores_case {
            ScopeCaseSemantics::IgnoresCase
        } else {
            ScopeCaseSemantics::Exact
        };
        assert_eq!(observed.write(0), expected);
        let existing = RepoScope {
            path: lower.clone(),
            recursive: false,
        };
        assert_eq!(
            scopes_overlap_observed(&existing, &candidate, observed.write(0)),
            ignores_case
        );

        let keys = repo_path_identity_keys(temp.path(), &[upper, lower]).unwrap();
        assert_eq!(keys[0] == keys[1], ignores_case);
        let duplicates = normalize_repo_scopes(
            temp.path(),
            &[
                RepoScope {
                    path: "src/New.rs".to_string(),
                    recursive: false,
                },
                RepoScope {
                    path: "src/new.rs".to_string(),
                    recursive: false,
                },
            ],
        );
        let rejected_as_duplicate = matches!(
            &duplicates,
            Err(StoreError::InvalidScope(message)) if message.contains("duplicate scope path")
        );
        assert_eq!(rejected_as_duplicate, ignores_case, "{duplicates:?}");
        // Unrelated spellings never need an observation and keep exact keys.
        let distinct = repo_path_identity_keys(
            temp.path(),
            &["src/a.rs".to_string(), "src/b.rs".to_string()],
        )
        .unwrap();
        assert_eq!(distinct, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
    }
}
