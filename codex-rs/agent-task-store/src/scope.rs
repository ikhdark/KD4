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
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or_else(|| {
                    StoreError::InvalidScope(format!("scope has no existing ancestor: {relative}"))
                })?;
            }
            Err(error) => return Err(StoreError::InvalidScope(format!(
                "scope ancestor {} cannot be inspected: {error}", existing.display()
            ))),
        }
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
    /// Lookups distinguish case.
    Exact,
    IgnoresCase,
    /// Unknown policy must not turn a possible writer collision into disjointness.
    #[default]
    Unknown,
}

/// Case semantics observed for an admission candidate's scopes before its
/// transaction opens, so admission never holds the database while it waits
/// on the filesystem.
#[derive(Clone, Debug, Default)]
pub(crate) struct CandidateScopeCase {
    read: Vec<Vec<ScopeCaseSemantics>>,
    write: Vec<Vec<ScopeCaseSemantics>>,
}

impl CandidateScopeCase {
    pub(crate) fn observe(
        repo_root: &Path,
        read_scope: &[RepoScope],
        write_scope: &[RepoScope],
    ) -> Self {
        let Ok(mut observer) = CaseObserver::new(repo_root) else {
            // Missing observations use conservative per-component comparison.
            return Self::default();
        };
        Self {
            read: observer.scopes(read_scope),
            write: observer.scopes(write_scope),
        }
    }

    pub(crate) fn read(&self, index: usize) -> &[ScopeCaseSemantics] {
        self.read.get(index).map(Vec::as_slice).unwrap_or_default()
    }

    pub(crate) fn write(&self, index: usize) -> &[ScopeCaseSemantics] {
        self.write.get(index).map(Vec::as_slice).unwrap_or_default()
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
    semantics: &[ScopeCaseSemantics],
) -> bool {
    if existing.overlaps(candidate) {
        return true;
    }
    let existing_key = component_comparison_key(&existing.path, semantics);
    let candidate_key = component_comparison_key(&candidate.path, semantics);
    existing_key == candidate_key
        || existing.recursive && is_descendant(&existing_key, &candidate_key)
        || candidate.recursive && is_descendant(&candidate_key, &existing_key)
}

/// Deduplication keys for normalized repository paths, in input order.
///
/// Normalization resolves existing aliases to the filesystem's spelling, so
/// only spellings that still differ by case alone can name one entry: a path
/// that does not exist yet, or one created after it was declared. Those keys
/// compare each component using its own parent directory's lookup policy.
/// Keys compare paths; persist the normalized paths themselves.
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
            if ambiguous {
                component_comparison_key(path, &observer.semantics(path))
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

    fn scopes(&mut self, scopes: &[RepoScope]) -> Vec<Vec<ScopeCaseSemantics>> {
        scopes
            .iter()
            .map(|scope| self.semantics(&scope.path))
            .collect()
    }

    /// Each component is looked up under its parent, not under the deepest
    /// child. Prospective directories inherit the last existing parent's mode.
    fn semantics(&mut self, normalized_path: &str) -> Vec<ScopeCaseSemantics> {
        let mut directory = self.canonical_root.clone();
        let mut inherited = ScopeCaseSemantics::Unknown;
        let mut semantics = Vec::new();
        for component in normalized_path.split('/') {
            if directory.is_dir() {
                inherited = *self.observed.entry(directory.clone())
                    .or_insert_with_key(|directory| observe_case_semantics(directory));
            }
            semantics.push(inherited);
            directory.push(component);
        }
        semantics
    }
}

fn component_comparison_key(path: &str, semantics: &[ScopeCaseSemantics]) -> String {
    path.split('/').enumerate().map(|(index, component)| {
        if semantics.get(index) == Some(&ScopeCaseSemantics::Exact) {
            component.to_string()
        } else {
            case_folded(component)
        }
    }).collect::<Vec<_>>().join("/")
}

fn observe_case_semantics(directory: &Path) -> ScopeCaseSemantics {
    if let Ok(Some(sensitive)) = codex_file_system::directory_case_sensitive(directory) {
        return if sensitive { ScopeCaseSemantics::Exact } else { ScopeCaseSemantics::IgnoresCase };
    }
    let observed = std::fs::read_dir(directory).ok().and_then(|entries| {
        entries
            .take(CASE_PROBE_ENTRY_LIMIT)
            .filter_map(Result::ok)
            // A symlink's target can satisfy both spellings in a case-sensitive directory.
            .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_symlink()))
            .find_map(|entry| lookup_ignores_case(directory, &entry.file_name()))
    });
    match observed {
        Some(true) => ScopeCaseSemantics::IgnoresCase,
        Some(false) => ScopeCaseSemantics::Exact,
        None => ScopeCaseSemantics::Unknown,
    }
}

/// Resolve an existing entry through its case-swapped spelling. A miss, or a
/// distinct sibling under that spelling, proves the lookup is case-sensitive.
fn lookup_ignores_case(directory: &Path, name: &std::ffi::OsStr) -> Option<bool> {
    let name = name.to_str()?;
    let original_metadata = std::fs::symlink_metadata(directory.join(name)).ok()?;
    if metadata_is_alias(&original_metadata) { return None; }
    let swapped = swap_ascii_case(name);
    if swapped == name {
        return None;
    }
    let swapped = directory.join(swapped);
    match std::fs::symlink_metadata(&swapped) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
        Ok(metadata) => {
            if metadata_is_alias(&metadata) { return None; }
            let original = std::fs::canonicalize(directory.join(name)).ok()?;
            let swapped = std::fs::canonicalize(&swapped).ok()?;
            Some(original == swapped)
        }
    }
}

fn metadata_is_alias(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
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
    fn component_policies_preserve_case_distinct_parents_and_conservative_unknowns() {
        let existing = RepoScope { path: "Dir/new.rs".into(), recursive: false };
        let candidate = RepoScope { path: "dir/New.rs".into(), recursive: false };
        let mixed = [ScopeCaseSemantics::Exact, ScopeCaseSemantics::IgnoresCase];
        assert!(!scopes_overlap_observed(&existing, &candidate, &mixed));
        assert_ne!(component_comparison_key(&existing.path, &mixed), component_comparison_key(&candidate.path, &mixed));
        let same_parent = RepoScope { path: "Dir/New.rs".into(), recursive: false };
        assert!(scopes_overlap_observed(&existing, &same_parent, &mixed));
        assert!(scopes_overlap_observed(&existing, &candidate, &[]), "unknown lookup policy cannot exclude a possible collision");
    }

    #[test]
    fn dangling_scope_aliases_are_not_lexically_restored() {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(normalize_repo_path(temp.path(), "ordinary/missing.rs").unwrap(), "ordinary/missing.rs");
        symlink_file(temp.path().join("missing-file"), temp.path().join("broken-file")).unwrap();
        symlink_dir(temp.path().join("missing-directory"), temp.path().join("broken-directory")).unwrap();
        for path in ["broken-file", "broken-directory", "broken-directory/child.rs"] {
            assert!(normalize_repo_path(temp.path(), path).is_err(), "{path}");
        }
    }

    #[test]
    fn swapped_aliases_do_not_prove_case_insensitive_lookup() {
        use std::os::windows::fs::symlink_file;
        let temp = tempfile::tempdir().unwrap();
        set_directory_case_sensitive(temp.path(), true);
        std::fs::write(temp.path().join("Owner.rs"), "owner").unwrap();
        symlink_file(temp.path().join("Owner.rs"), temp.path().join("oWNER.RS")).unwrap();
        assert_eq!(lookup_ignores_case(temp.path(), std::ffi::OsStr::new("Owner.rs")), None);
        assert_eq!(observe_case_semantics(temp.path()), ScopeCaseSemantics::Exact);
    }

    fn set_directory_case_sensitive(path: &Path, sensitive: bool) {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::*;
        let directory = std::fs::OpenOptions::new().read(true).access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path).unwrap();
        let info = FILE_CASE_SENSITIVE_INFO { Flags: u32::from(sensitive) };
        // SAFETY: directory owns the live handle; info has the exact required
        // layout and remains valid until this synchronous call returns.
        let result = unsafe { SetFileInformationByHandle(directory.as_raw_handle().cast(), FileCaseSensitiveInfo,
            (&info as *const FILE_CASE_SENSITIVE_INFO).cast(), std::mem::size_of::<FILE_CASE_SENSITIVE_INFO>() as u32) };
        assert_ne!(result, 0, "case mode fixture: {}", std::io::Error::last_os_error());
    }

    #[test]
    fn empty_child_directory_uses_its_own_policy_and_mixed_parent_identity() {
        let temp = tempfile::tempdir().unwrap();
        set_directory_case_sensitive(temp.path(), true);
        for name in ["Dir", "dir"] {
            let child = temp.path().join(name);
            std::fs::create_dir(&child).unwrap();
            set_directory_case_sensitive(&child, false);
            assert_eq!(observe_case_semantics(&child), ScopeCaseSemantics::IgnoresCase);
        }
        let paths = vec!["Dir/new.rs".into(), "dir/New.rs".into()];
        let keys = repo_path_identity_keys(temp.path(), &paths).unwrap();
        assert_ne!(keys[0], keys[1]);
        let scopes: Vec<_> = paths.into_iter().map(|path| RepoScope { path, recursive: false }).collect();
        assert!(normalize_repo_scopes(temp.path(), &scopes).is_ok());
        let observation = CandidateScopeCase::observe(temp.path(), &[], &scopes);
        assert!(!scopes_overlap_observed(&scopes[0], &scopes[1], observation.write(1)));
        let insensitive = tempfile::tempdir().unwrap();
        set_directory_case_sensitive(insensitive.path(), false);
        let child = insensitive.path().join("sensitive");
        std::fs::create_dir(&child).unwrap();
        set_directory_case_sensitive(&child, true);
        assert_eq!(observe_case_semantics(&child), ScopeCaseSemantics::Exact);
    }

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
        assert!(!scopes_overlap_observed(&existing, &candidate, &[ScopeCaseSemantics::Exact; 2]));
        assert!(!scopes_overlap_observed(&directory, &candidate, &[ScopeCaseSemantics::Exact; 2]));
        assert!(scopes_overlap_observed(&existing, &candidate, &[ScopeCaseSemantics::IgnoresCase; 2]));
        assert!(scopes_overlap_observed(&directory, &candidate, &[ScopeCaseSemantics::IgnoresCase; 2]));
        // Folding never merges distinct names, and keeps one-to-one mappings.
        assert!(!scopes_overlap_observed(&directory, &sibling, &[ScopeCaseSemantics::IgnoresCase; 2]));
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
        assert_eq!(observed.write(0).last(), Some(&expected));
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
