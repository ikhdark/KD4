mod atomic_write;
mod conditional_mutation;
mod confined_file;
mod copy_failure;
mod directory_guard;
mod find_up;
mod text_diagnostics;
mod walk_gitignore;

pub use conditional_mutation::FileMutationOutcome;
pub use conditional_mutation::guard_patch_read;
pub use conditional_mutation::mutate_file_if_matches;
pub use conditional_mutation::prepare_patch_capture;
pub use conditional_mutation::prepare_patch_capture_namespace;
pub use conditional_mutation::try_acquire_patch_capture;
pub use confined_file::directory_case_sensitive;
pub use confined_file::open_confined_file;
pub use confined_file::open_confined_file_with_metadata;
pub use confined_file::resolved_file_path;
pub use copy_failure::CopyFailure;
pub use directory_guard::lock_directory_chain;
pub use find_up::FindUpErrorPolicy;
pub use find_up::find_nearest_ancestor_with_markers;
pub use find_up::find_nearest_native_ancestor_with_markers;
pub use text_diagnostics::DecodingError;
pub use text_diagnostics::DecodingErrorKind;
pub use text_diagnostics::EncodingDetection;
pub use text_diagnostics::FinalNewline;
pub use text_diagnostics::LineEndings;
pub use text_diagnostics::TextDiagnostics;
pub use text_diagnostics::TextDiagnosticsScanner;

use bytes::Bytes;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::ManagedFileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::SandboxEnforcement;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxKind;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::SandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::Stream;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

/// Maximum chunk size returned by [`ExecutorFileSystem::read_file_stream`].
pub const FILE_READ_CHUNK_SIZE: usize = 1024 * 1024;
/// Maximum payload of one revision-bound seek read (not a whole-file snapshot).
pub const FILE_RANGE_MAX_BYTES: usize = 64 * 1024;

/// Read-only, point-in-time name observations; never permission or revision guards.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FileSystemLookup {
    Canonicalize { path: PathUri },
    FirstRegularFile { candidates: Vec<PathUri> },
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FileSystemLookupResult {
    Resolved { path: PathUri },
    Missing,
    Inaccessible { error: String },
    Unsupported { error: String },
    Invalid { error: String },
    Unknown { error: String },
}

impl FileSystemLookupResult {
    pub fn from_result(result: io::Result<Option<PathUri>>) -> Self {
        match result {
            Ok(Some(path)) => Self::Resolved { path },
            Ok(None) => Self::Missing,
            Err(error) => {
                let message = error.to_string();
                match error.kind() {
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => Self::Missing,
                    io::ErrorKind::PermissionDenied => Self::Inaccessible { error: message },
                    _ if cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)) => {
                        Self::Inaccessible { error: message }
                    }
                    io::ErrorKind::Unsupported => Self::Unsupported { error: message },
                    io::ErrorKind::InvalidInput => Self::Invalid { error: message },
                    _ => Self::Unknown { error: message },
                }
            }
        }
    }
}

pub fn validate_lookup_batch(requests: &[FileSystemLookup]) -> io::Result<()> {
    let mut probes = 0usize;
    let mut bytes = 0usize;
    for request in requests {
        let paths = match request {
            FileSystemLookup::Canonicalize { path } => std::slice::from_ref(path),
            FileSystemLookup::FirstRegularFile { candidates } => candidates.as_slice(),
        };
        probes = probes.saturating_add(paths.len());
        for path in paths {
            bytes = bytes.saturating_add(path.as_str().len());
        }
    }
    if requests.is_empty() || requests.len() > 32 || probes > 16_384 || bytes > 4 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            "lookup batches require 1-32 requests, at most 16384 probes and 4 MiB of path text"));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRangeRead {
    pub bytes: Vec<u8>,
    pub file_bytes: u64,
    /// Opaque opened-object identity and metadata revision, not a content hash.
    pub revision: String,
}

/// SHA-256 of every byte of one opened regular file; no bytes are retained.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDigest {
    /// Lowercase hexadecimal SHA-256.
    pub sha256: String,
    pub file_bytes: u64,
    /// The same opaque opened-object revision as [`FileRangeRead::revision`].
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_diagnostics: Option<TextDiagnostics>,
}
const MAX_WALK_DEPTH: usize = 64;
const MAX_WALK_DIRECTORIES: usize = 10_000;
const MAX_WALK_ENTRIES: usize = 50_000;
pub const MAX_WALK_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const WALK_RESPONSE_ITEM_OVERHEAD_BYTES: usize = 64;
const MAX_CONCURRENT_WALK_METADATA: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateDirectoryOptions {
    pub recursive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoveOptions {
    pub recursive: bool,
    pub force: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CopyOptions {
    pub recursive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMetadata {
    pub is_directory: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    /// Size in bytes.
    pub size: u64,
    pub created_at_ms: i64,
    pub modified_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadDirectoryEntry {
    pub file_name: String,
    pub is_directory: bool,
    pub is_file: bool,
}

/// A producer-bounded directory read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadDirectoryOutcome<T = ReadDirectoryEntry> {
    pub entries: Vec<T>,
    /// Number of raw directory entries consumed from the filesystem.
    pub entries_examined: usize,
    /// Whether the read stopped because it exhausted its entry budget.
    ///
    /// This is conservative: it may be true when the directory contains
    /// exactly `entries_examined` entries.
    pub limit_reached: bool,
}

/// Walk classification captured by the enumerating backend, when available.
/// This is an observation, not a stable snapshot or an authorization decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WalkDirectoryEntry {
    Entry {
        file_name: String,
        metadata: Option<WalkEntryMetadata>,
    },
    /// Exceptional locators are boxed so ordinary batches retain their size.
    NativeEntry(Box<NativeWalkDirectoryEntry>),
    /// An enumeration failure with its exact operational child identity.
    Error { path: PathUri, message: String },
    /// Display-only detail; it must never be joined or probed as a native path.
    UnrepresentableName { escaped_name: String },
}

/// A native child whose operational identity cannot be reconstructed from a
/// Unicode basename and its parent URI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeWalkDirectoryEntry {
    pub path: PathUri,
    /// Display-only when `name_is_lossless` is false.
    pub file_name: String,
    pub name_is_lossless: bool,
    pub metadata: Option<WalkEntryMetadata>,
}

impl WalkDirectoryEntry {
    fn sort_name(&self) -> &str {
        match self {
            Self::Entry { file_name, .. } => file_name,
            Self::NativeEntry(entry) => &entry.file_name,
            Self::Error { path, .. } => path.as_str(),
            Self::UnrepresentableName { escaped_name } => escaped_name,
        }
    }
}

/// Classification is always retained; optional details are boxed so ordinary
/// walks do not allocate or retain full metadata for every entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalkEntryMetadata {
    pub is_directory: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub details: Option<Box<WalkFileMetadata>>,
}

/// Cheap enumeration metadata, never content/MIME detection or target probing.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct WalkFileMetadata {
    pub size: u64,
    pub created_at_ms: Option<i64>,
    pub modified_at_ms: Option<i64>,
    pub readonly: bool,
    pub is_symlink: bool,
}

impl From<FileMetadata> for WalkEntryMetadata {
    fn from(metadata: FileMetadata) -> Self {
        Self {
            is_directory: metadata.is_directory,
            is_file: metadata.is_file,
            is_symlink: metadata.is_symlink,
            details: None,
        }
    }
}

/// Bounds for a recursive filesystem walk.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkOptions {
    /// Maximum directory depth below the root that may be traversed.
    pub max_depth: usize,
    /// Maximum number of directories that may be traversed, including the root.
    pub max_directories: usize,
    /// Maximum number of directory entries that may be examined. With include
    /// filters it instead bounds the matching files returned; examined entries
    /// then remain bounded by the walk's hard maximum.
    pub max_entries: usize,
    /// Whether directory symlinks should be followed.
    pub follow_directory_symlinks: bool,
    /// Aggregate during traversal instead of retaining individual entry paths.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub summary_only: bool,
    /// Retain cheap entry metadata from enumeration; default false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_metadata: bool,
    /// Whether directories whose names start with `.` should be returned but not traversed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prune_hidden_directories: bool,
    /// Apply ancestor and nested `.gitignore` files inside Git repositories only,
    /// independently of hidden traversal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub apply_gitignore: bool,
    #[serde(default, skip_serializing_if = "WalkFilters::is_empty")]
    pub filters: WalkFilters,
}

/// Case-sensitive basename wildcards, independent of platform and gitignore.
/// `*` matches zero or more Unicode characters; `?` matches one. Everything
/// else is literal. File filters never prune directories needed for coverage;
/// with include patterns, directories are traversed but not returned. Explicit
/// path patterns are anchored to the selected root, accept `/` or `\` separators,
/// and match case-sensitively. `*` and `?` stay within a segment; a whole-segment
/// `**` matches zero or more segments. Other characters are literal. Include
/// groups are unioned; either exclusion group wins. Only directory exclusions
/// prune traversal; the selected root is never excluded.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WalkFilters {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_directories: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_directory_paths: Vec<String>,
}

impl WalkFilters {
    pub fn is_empty(&self) -> bool {
        !self.has_file_includes()
            && self.exclude.is_empty()
            && self.exclude_directories.is_empty()
            && self.exclude_paths.is_empty()
            && self.exclude_directory_paths.is_empty()
    }

    pub fn has_file_includes(&self) -> bool {
        !self.include.is_empty() || !self.include_paths.is_empty()
    }

    fn has_path_patterns(&self) -> bool {
        !self.include_paths.is_empty()
            || !self.exclude_paths.is_empty()
            || !self.exclude_directory_paths.is_empty()
    }

    pub fn validate(&self) -> io::Result<()> {
        let basename_patterns = self
            .include
            .iter()
            .chain(&self.exclude)
            .chain(&self.exclude_directories);
        let path_patterns = self
            .include_paths
            .iter()
            .chain(&self.exclude_paths)
            .chain(&self.exclude_directory_paths);
        if basename_patterns.clone().count() + path_patterns.clone().count() > 64
            || basename_patterns.into_iter().any(|pattern| {
                pattern.is_empty() || pattern.len() > 256 || pattern.contains(['/', '\\', '\0'])
            })
            || path_patterns.into_iter().any(|pattern| {
                pattern.is_empty()
                    || pattern.len() > 256
                    || pattern.contains(['\0', ':'])
                    || pattern.split(['/', '\\']).any(|segment| {
                        segment.is_empty()
                            || segment == "."
                            || segment == ".."
                            || (segment.contains("**") && segment != "**")
                    })
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "walk filters require at most 64 nonempty patterns, each at most 256 UTF-8 bytes; basename patterns forbid separators and NUL; path patterns must be root-relative with / or \\ separators, no empty/dot/parent segments, colon or NUL, and ** only as a whole segment",
            ));
        }
        Ok(())
    }
}

struct PreparedWalkFilters {
    include: Vec<Vec<char>>,
    exclude: Vec<Vec<char>>,
    exclude_directories: Vec<Vec<char>>,
    include_paths: Vec<Vec<Vec<char>>>,
    exclude_paths: Vec<Vec<Vec<char>>>,
    exclude_directory_paths: Vec<Vec<Vec<char>>>,
}

impl PreparedWalkFilters {
    fn new(filters: &WalkFilters) -> Self {
        let prepare = |patterns: &[String]| -> Vec<Vec<char>> {
            patterns
                .iter()
                .map(|pattern| pattern.chars().collect())
                .collect()
        };
        let prepare_paths = |patterns: &[String]| -> Vec<Vec<Vec<char>>> {
            patterns
                .iter()
                .map(|pattern| {
                    pattern
                        .split(['/', '\\'])
                        .map(|segment| segment.chars().collect())
                        .collect()
                })
                .collect()
        };
        Self {
            include: prepare(&filters.include),
            exclude: prepare(&filters.exclude),
            exclude_directories: prepare(&filters.exclude_directories),
            include_paths: prepare_paths(&filters.include_paths),
            exclude_paths: prepare_paths(&filters.exclude_paths),
            exclude_directory_paths: prepare_paths(&filters.exclude_directory_paths),
        }
    }

    fn includes_file(&self, name: &[char], path: &[&[char]]) -> bool {
        ((self.include.is_empty() && self.include_paths.is_empty())
            || self
                .include
                .iter()
                .any(|pattern| basename_matches(pattern, name))
            || self
                .include_paths
                .iter()
                .any(|pattern| relative_path_matches(pattern, path)))
            && !self
                .exclude
                .iter()
                .any(|pattern| basename_matches(pattern, name))
            && !self
                .exclude_paths
                .iter()
                .any(|pattern| relative_path_matches(pattern, path))
    }
}

fn relative_path_matches(pattern: &[Vec<char>], path: &[&[char]]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut globstar = None;
    let mut retry = 0;
    while n < path.len() {
        if p < pattern.len() && pattern[p].as_slice() == ['*', '*'] {
            globstar = Some(p);
            p += 1;
            retry = n;
        } else if p < pattern.len() && basename_matches(&pattern[p], path[n]) {
            p += 1;
            n += 1;
        } else if let Some(index) = globstar {
            retry += 1;
            n = retry;
            p = index + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p].as_slice() == ['*', '*'] {
        p += 1;
    }
    p == pattern.len()
}

fn basename_matches(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star = None;
    let mut retry = 0;
    while n < name.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            retry = n;
        } else if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if let Some(index) = star {
            retry += 1;
            n = retry;
            p = index + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Type of a filesystem entry returned by a walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WalkEntryKind {
    Directory,
    File,
}

/// One entry returned by a walk.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkEntry {
    pub path: PathUri,
    pub kind: WalkEntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Box<WalkFileMetadata>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_error: Option<String>,
}

/// A descendant that could not be inspected during a walk.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkError {
    pub path: PathUri,
    pub message: String,
}

/// A scope to re-examine, not a resumable offset into a mutable directory.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkStop {
    pub path: PathUri,
    pub reason: String,
    pub effective_limit: usize,
    pub partially_examined: bool,
}

/// Totals for eligible descendants observed during the same bounded scan.
/// The root is excluded. Bytes are logical file sizes counted per path,
/// not unique objects or allocated storage. Errors/cutoffs make totals partial.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkSummary {
    pub files: usize,
    pub directories: usize,
    pub file_bytes: u64,
    pub entries_examined: usize,
    pub directories_read: usize,
    /// Observed exclusions only, not unknown descendants of skipped directories.
    /// Inaccessible/invalid entries retain their paths in WalkOutcome::errors.
    pub skipped: BTreeMap<String, usize>,
}

/// Entries and recoverable errors collected by a bounded walk.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkOutcome {
    /// Acknowledges handle-bound no-follow enumeration, not a frozen tree.
    /// Absence from older peers must never certify strict source scope.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_follow_enforced: bool,
    /// Older peers omit this and cannot certify gitignore-filtered scope.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gitignore_applied: bool,
    /// Observed entries omitted by gitignore rules; an ignored directory counts
    /// once and its contents are not examined.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gitignored_entries: usize,
    /// Older peers cannot silently acknowledge requested metadata.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub metadata_included: bool,
    /// Presence acknowledges summary-only execution, including on empty trees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<WalkSummary>,
    pub entries: Vec<WalkEntry>,
    pub errors: Vec<WalkError>,
    /// Whether a traversal budget prevented inspecting all eligible descendants.
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unexplored: Vec<WalkStop>,
    /// Echo only when filters were applied. Older remote backends omit this;
    /// their unfiltered response must not certify a filtered requested scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_filters: Option<WalkFilters>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemSandboxContext {
    pub permissions: PermissionProfile<PathUri>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathUri>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_roots: Vec<PathUri>,
    pub windows_sandbox_level: WindowsSandboxLevel,
    #[serde(default)]
    pub windows_sandbox_private_desktop: bool,
}

impl FileSystemSandboxContext {
    pub fn from_legacy_sandbox_policy(
        sandbox_policy: SandboxPolicy,
        cwd: PathUri,
    ) -> io::Result<Self> {
        // Legacy policy projection materializes native roots, so convert at the receiving-host
        // boundary while retaining the URI in the resulting sandbox context.
        let native_cwd = cwd.to_abs_path()?;
        let file_system_sandbox_policy =
            FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
                &sandbox_policy,
                &native_cwd,
            );
        let permissions =
            PermissionProfile::<AbsolutePathBuf>::from_runtime_permissions_with_enforcement(
                SandboxEnforcement::from_legacy_sandbox_policy(&sandbox_policy),
                &file_system_sandbox_policy,
                NetworkSandboxPolicy::from(&sandbox_policy),
            );
        Ok(Self::from_permission_profile_with_cwd(permissions, cwd))
    }

    pub fn from_permission_profile(permissions: PermissionProfile<AbsolutePathBuf>) -> Self {
        Self::from_permissions_and_cwd(permissions, /*cwd*/ None)
    }

    pub fn from_permission_profile_with_cwd(
        permissions: PermissionProfile<AbsolutePathBuf>,
        cwd: PathUri,
    ) -> Self {
        Self::from_permissions_and_cwd(permissions, Some(cwd))
    }

    fn from_permissions_and_cwd(
        permissions: PermissionProfile<AbsolutePathBuf>,
        cwd: Option<PathUri>,
    ) -> Self {
        Self {
            permissions: permissions.into(),
            cwd,
            workspace_roots: Vec::new(),
            windows_sandbox_level: WindowsSandboxLevel::Disabled,
            windows_sandbox_private_desktop: false,
        }
    }

    pub fn should_run_in_sandbox(&self) -> bool {
        let Ok(permissions) =
            PermissionProfile::<AbsolutePathBuf>::try_from(self.permissions.clone())
        else {
            // A sandbox context for another host must not select the unsandboxed filesystem.
            return true;
        };
        let file_system_policy = permissions.file_system_sandbox_policy();
        matches!(file_system_policy.kind, FileSystemSandboxKind::Restricted)
            && !file_system_policy.has_full_disk_write_access()
    }

    pub fn has_cwd_dependent_permissions(&self) -> bool {
        match &self.permissions {
            PermissionProfile::Managed {
                file_system: ManagedFileSystemPermissions::Restricted { entries, .. },
                ..
            } => entries.iter().any(|entry| match &entry.path {
                FileSystemPath::GlobPattern { pattern } => !Path::new(pattern).is_absolute(),
                FileSystemPath::Special {
                    value: FileSystemSpecialPath::ProjectRoots { .. },
                } => true,
                FileSystemPath::Path { .. } | FileSystemPath::Special { .. } => false,
            }),
            PermissionProfile::Managed {
                file_system: ManagedFileSystemPermissions::Unrestricted,
                ..
            }
            | PermissionProfile::Disabled
            | PermissionProfile::External { .. } => false,
        }
    }

    pub fn drop_cwd_if_unused(mut self) -> Self {
        if !self.has_cwd_dependent_permissions() {
            self.cwd = None;
            self.workspace_roots.clear();
        }
        self
    }
}

pub type FileSystemResult<T> = io::Result<T>;

/// Future returned by [`ExecutorFileSystem`] operations.
pub type ExecutorFileSystemFuture<'a, T> =
    Pin<Box<dyn Future<Output = FileSystemResult<T>> + Send + 'a>>;

/// Stream of immutable chunks read from an [`ExecutorFileSystem`].
pub struct FileSystemReadStream {
    inner: Pin<Box<dyn Stream<Item = FileSystemResult<Bytes>> + Send + 'static>>,
}

impl FileSystemReadStream {
    /// Wraps a filesystem byte stream.
    pub fn new(stream: impl Stream<Item = FileSystemResult<Bytes>> + Send + 'static) -> Self {
        Self {
            inner: Box::pin(stream),
        }
    }

    /// Preserve a collecting read's limit while allowing a metadata consumer
    /// to discard body chunks. No successful prefix certifies an oversized file.
    pub fn with_limit(self, max_bytes: u64) -> Self {
        Self::new(self.scan(Some(0_u64), move |used, chunk| {
            let Some(current) = used.as_mut() else {
                return std::future::ready(None);
            };
            let result = chunk.and_then(|bytes| {
                if bytes.len() as u64 > max_bytes.saturating_sub(*current) {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("file exceeds the {max_bytes}-byte read limit"),
                    ))
                } else {
                    *current += bytes.len() as u64;
                    Ok(bytes)
                }
            });
            if result.is_err() {
                *used = None;
            }
            std::future::ready(Some(result))
        }))
    }
}

impl Stream for FileSystemReadStream {
    type Item = FileSystemResult<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

async fn collect_bounded_read(
    mut stream: FileSystemReadStream,
    max_bytes: usize,
) -> FileSystemResult<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.len() > max_bytes.saturating_sub(bytes.len()) {
            return Ok(None);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(bytes))
}

/// Abstract filesystem access used by components that may operate locally or via
/// a remote environment.
pub trait ExecutorFileSystem: Send + Sync {
    /// Ordered independent results, including duplicates. Isolation backends
    /// override this to use one helper/RPC, without changing probe ordering.
    fn lookup_batch<'a>(
        &'a self,
        requests: &'a [FileSystemLookup],
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<FileSystemLookupResult>> {
        Box::pin(async move {
            validate_lookup_batch(requests)?;
            let mut results = Vec::with_capacity(requests.len());
            for request in requests {
                let result = match request {
                    FileSystemLookup::Canonicalize { path } => self.canonicalize(path, sandbox).await.map(Some),
                    FileSystemLookup::FirstRegularFile { candidates } => self.first_regular_file(candidates, sandbox).await,
                };
                results.push(FileSystemLookupResult::from_result(result));
            }
            Ok(results)
        })
    }

    /// Whether native paths name this Codex process's host filesystem. Foreign
    /// Windows paths are not host paths merely because their syntax matches.
    fn uses_host_native_paths(&self) -> bool {
        false
    }

    /// Child-name case policy of an existing directory in this filesystem.
    /// `None` means unknown, not insensitive. This observation is not permanent.
    fn directory_case_sensitive<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<bool>> {
        Box::pin(async move {
            let _ = (path, sandbox);
            Ok(None)
        })
    }

    /// Resolves a path within this filesystem.
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri>;

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>>;

    /// Reads a file as a stream of chunks no larger than [`FILE_READ_CHUNK_SIZE`].
    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream>;

    /// Metadata-only consumer stream with the same accepted size/sandbox policy
    /// as `read_file`. Implementations may stream; the safe default preserves
    /// custom collecting readers rather than bypassing their admission limits.
    fn read_file_metadata_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async move {
            let bytes = Bytes::from(self.read_file(path, sandbox).await?);
            Ok(FileSystemReadStream::new(futures::stream::once(
                std::future::ready(Ok(bytes)),
            )))
        })
    }

    /// Read at most FILE_RANGE_MAX_BYTES at an absolute byte offset. The backend
    /// must detect opened-object changes during the read and reject a mismatched
    /// expected revision before returning bytes. Never emulate this by collecting
    /// the file or by returning an unversioned stream.
    fn read_file_range<'a>(
        &'a self,
        path: &'a PathUri,
        start: u64,
        len: usize,
        expected_revision: Option<&'a str>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileRangeRead> {
        Box::pin(async move {
            let _ = (path, start, len, expected_revision, sandbox);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "revision-bound byte ranges are not supported by this filesystem",
            ))
        })
    }

    /// Hash every byte of one opened regular file with SHA-256 in a single
    /// backend operation. `None` means the object changed during acquisition or
    /// the path no longer names it afterwards. Never emulate this with per-page
    /// reads or by collecting the file.
    fn checksum_file<'a>(
        &'a self,
        path: &'a PathUri,
        include_text_diagnostics: bool,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<FileDigest>> {
        Box::pin(async move {
            let _ = (path, include_text_diagnostics, sandbox);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "streaming file checksums are not supported by this filesystem",
            ))
        })
    }

    /// The first candidate that is a regular file (following links), in order,
    /// spelled with its stored file-name case when the backend reports it.
    /// Missing candidates are skipped; any other probe error stops the search.
    /// Backends with per-operation isolation costs must override this to probe
    /// the whole list under one policy decision.
    fn first_regular_file<'a>(
        &'a self,
        candidates: &'a [PathUri],
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<PathUri>> {
        Box::pin(async move {
            for candidate in candidates {
                let metadata = match self.get_metadata(candidate, sandbox).await {
                    Ok(metadata) => metadata,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => {
                        // Wrapping the diagnostic drops raw_os_error; retain
                        // Windows sharing-denial semantics in the portable kind.
                        let kind = if cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)) {
                            io::ErrorKind::PermissionDenied
                        } else {
                            error.kind()
                        };
                        return Err(io::Error::new(
                            kind,
                            format!(
                                "unable to inspect candidate {}: {error}",
                                candidate.inferred_native_path_string()
                            ),
                        ));
                    }
                };
                if !metadata.is_file {
                    continue;
                }
                // Report git.exe rather than a PATHEXT spelling such as git.EXE.
                // A link keeps its own name; canonicalization names its target.
                let stored = if metadata.is_symlink {
                    None
                } else {
                    self.canonicalize(candidate, sandbox)
                        .await
                        .ok()
                        .and_then(|canonical| canonical.basename())
                        .zip(candidate.basename())
                        .filter(|(stored, requested)| {
                            stored != requested && stored.eq_ignore_ascii_case(requested)
                        })
                        .and_then(|(stored, _)| candidate.parent()?.join(&stored).ok())
                };
                return Ok(Some(stored.unwrap_or_else(|| candidate.clone())));
            }
            Ok(None)
        })
    }

    /// Performs two bounded reads and returns at most `max_bytes` when they agree.
    ///
    /// The default checks byte agreement, not atomicity or stable object identity.
    /// Returns `None` if the file exceeds the limit or two bounded reads do not
    /// return identical bytes. Implementations that can also inspect native file
    /// metadata or identity should override this method to reject replacements
    /// and changes observed during either read.
    fn read_file_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        self.read_file_bounded_via_streams(path, max_bytes, sandbox)
    }

    /// A bounded acquisition with optional opened-object provenance. Absence of
    /// that provenance must not be used to certify a canonical inspection target.
    fn read_file_bounded_with_source<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<(Vec<u8>, Option<std::path::PathBuf>)>> {
        Box::pin(async move {
            Ok(self
                .read_file_bounded(path, max_bytes, sandbox)
                .await?
                .map(|bytes| (bytes, None)))
        })
    }

    /// Performs the default bounded read using two [`Self::read_file_stream`] passes.
    ///
    /// Overrides can use this when a backend lacks a native bounded read.
    fn read_file_bounded_via_streams<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let Some(first) =
                collect_bounded_read(self.read_file_stream(path, sandbox).await?, max_bytes)
                    .await?
            else {
                return Ok(None);
            };
            let mut stream = self.read_file_stream(path, sandbox).await?;
            let mut offset = 0usize;
            let mut matches = true;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                if chunk.len() > max_bytes.saturating_sub(offset) {
                    return Ok(None);
                }
                let end = offset + chunk.len();
                matches &= first.get(offset..end) == Some(chunk.as_ref());
                offset = end;
                // Continue after a mismatch so later I/O errors are still surfaced.
            }
            Ok((matches && offset == first.len()).then_some(first))
        })
    }

    /// Reads a stable bounded snapshot while keeping every open beneath `root`.
    ///
    /// Implementations must override this method with handle-relative,
    /// opened-handle, or server-side confinement. The default fails closed.
    fn read_file_bounded_confined<'a>(
        &'a self,
        path: &'a PathUri,
        root: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let _ = (path, root, max_bytes, sandbox);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "confined bounded reads are not implemented by this filesystem",
            ))
        })
    }

    /// Reads a file and decodes it as UTF-8 text.
    fn read_file_text<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, String> {
        Box::pin(async move {
            let bytes = self.read_file(path, sandbox).await?;
            String::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()>;

    /// Enforce the expected preimage at the mutation boundary, or refuse.
    /// No default read/check/write fallback: it would lose late external edits.
    /// None is expected absence / requested deletion, respectively.
    fn mutate_file_if_matches<'a>(
        &'a self,
        path: &'a PathUri,
        expected: Option<Vec<u8>>,
        replacement: Option<Vec<u8>>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMutationOutcome> {
        Box::pin(async move {
            let _ = (path, expected, replacement, sandbox);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "conditional file mutation is not supported by this filesystem",
            ))
        })
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        create_directory_options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()>;

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata>;

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>>;

    /// Reads at most `max_entries` raw directory entries without first
    /// materializing the complete directory.
    ///
    /// The default fails closed because implementing this in terms of
    /// [`Self::read_directory`] would not enforce the producer-side bound.
    fn read_directory_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_entries: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome> {
        Box::pin(async move {
            let _ = (path, max_entries, sandbox);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "bounded directory reads are not implemented by this filesystem",
            ))
        })
    }

    /// Like [`Self::read_directory_bounded`], optionally retaining link-aware
    /// metadata already obtained during enumeration. Backends without that
    /// information retain the walker's bounded metadata-probe fallback.
    fn read_directory_bounded_for_walk<'a>(
        &'a self,
        path: &'a PathUri,
        max_entries: usize,
        _include_metadata: bool,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome<WalkDirectoryEntry>> {
        Box::pin(async move {
            let batch = self
                .read_directory_bounded(path, max_entries, sandbox)
                .await?;
            Ok(ReadDirectoryOutcome {
                entries: batch
                    .entries
                    .into_iter()
                    .map(|entry| WalkDirectoryEntry::Entry {
                        file_name: entry.file_name,
                        metadata: None,
                    })
                    .collect(),
                entries_examined: batch.entries_examined,
                limit_reached: batch.limit_reached,
            })
        })
    }

    /// Bounded enumeration that pins the directory and its ancestors without
    /// following redirects. Unsupported backends must refuse, never lstat/read_dir.
    fn read_directory_bounded_no_follow_for_walk<'a>(
        &'a self,
        path: &'a PathUri,
        max_entries: usize,
        include_metadata: bool,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome<WalkDirectoryEntry>> {
        Box::pin(async move {
            let _ = (path, max_entries, include_metadata, sandbox);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no-follow directory enumeration is not supported by this filesystem",
            ))
        })
    }

    /// Recursively lists descendants, optionally following directory symlinks.
    fn walk<'a>(
        &'a self,
        path: &'a PathUri,
        options: WalkOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        self.walk_via_directory_reads(path, options, sandbox)
    }

    /// Performs a bounded walk using the primitive filesystem operations.
    ///
    /// Requires producer-bounded directory reads; unsupported backends fail explicitly.
    /// Each bounded batch is sorted, but a truncated walk need not contain the
    /// globally first names in a directory.
    fn walk_via_directory_reads<'a>(
        &'a self,
        path: &'a PathUri,
        options: WalkOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        Box::pin(walk_via_directory_reads(self, path, options, sandbox))
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        remove_options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()>;

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        copy_options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()>;
}

async fn walk_via_directory_reads<F: ExecutorFileSystem + ?Sized>(
    file_system: &F,
    root: &PathUri,
    options: WalkOptions,
    sandbox: Option<&FileSystemSandboxContext>,
) -> FileSystemResult<WalkOutcome> {
    options.filters.validate()?;
    if options.max_directories == 0 || options.max_entries == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "filesystem walk limits must be greater than zero",
        ));
    }
    if options.max_depth > MAX_WALK_DEPTH
        || options.max_directories > MAX_WALK_DIRECTORIES
        || options.max_entries > MAX_WALK_ENTRIES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "filesystem walk limits exceed maximums: depth={MAX_WALK_DEPTH}, directories={MAX_WALK_DIRECTORIES}, entries={MAX_WALK_ENTRIES}"
            ),
        ));
    }

    let root_identity = if options.follow_directory_symlinks {
        if !file_system.get_metadata(root, sandbox).await?.is_directory {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "walk root is not a directory",
            ));
        }
        file_system.canonicalize(root, sandbox).await?
    } else {
        // The first guarded enumeration validates the root without a
        // target-following metadata query (including a root link's target).
        root.clone()
    };
    let mut outcome = WalkOutcome {
        no_follow_enforced: !options.follow_directory_symlinks,
        gitignore_applied: options.apply_gitignore,
        metadata_included: options.include_metadata,
        summary: options.summary_only.then(WalkSummary::default),
        applied_filters: (!options.filters.is_empty()).then(|| options.filters.clone()),
        ..WalkOutcome::default()
    };
    let mut gitignore = None;
    // Retain lexical root-relative names, not canonical paths or URI spellings.
    // A non-Unicode ancestor leaves the path unknown instead of matching its
    // replacement label. Basename-only walks do not allocate relative paths.
    let mut queue = VecDeque::from([(
        root.clone(),
        0usize,
        options.filters.has_path_patterns().then(String::new),
    )]);
    let mut visited_directories = HashSet::from([root_identity]);
    let mut directory_count = 1usize;
    let mut entry_count = 0usize;
    let mut response_bytes = 0usize;
    let mut returned_entries = 0usize;
    // Include patterns ask for matching files only: directories are traversed
    // for coverage but not returned, and max_entries bounds returned matches
    // rather than misses. The hard maximum still bounds raw examination, so a
    // sparse match set cannot make the walk unbounded on a huge tree.
    let matching_files_only = options.filters.has_file_includes();
    let (examination_limit, examination_reason) = if matching_files_only {
        (MAX_WALK_ENTRIES, "examination_limit")
    } else {
        (options.max_entries, "entry_limit")
    };
    let filters = PreparedWalkFilters::new(&options.filters);
    let mut name_chars = Vec::new();
    let mut path_chars = Vec::new();

    while let Some((directory, depth, relative_directory)) = queue.pop_front() {
        if entry_count == examination_limit {
            return Ok(finish_stopped_walk(
                outcome,
                root,
                queue,
                directory,
                false,
                examination_reason,
                examination_limit,
            ));
        }
        let read = if options.follow_directory_symlinks {
            file_system.read_directory_bounded_for_walk(
                &directory,
                examination_limit - entry_count,
                options.include_metadata || options.summary_only,
                sandbox,
            )
        } else {
            file_system.read_directory_bounded_no_follow_for_walk(
                &directory,
                examination_limit - entry_count,
                options.include_metadata || options.summary_only,
                sandbox,
            )
        };
        let batch = match read.await {
            Ok(batch) => batch,
            Err(error) if &directory == root => return Err(error),
            Err(error) if error.kind() == io::ErrorKind::Unsupported => return Err(error),
            Err(error) => {
                if !push_walk_error(
                    &mut outcome,
                    &mut response_bytes,
                    directory.clone(),
                    error.to_string(),
                ) {
                    return Ok(finish_stopped_walk(
                        outcome,
                        root,
                        queue,
                        directory,
                        true,
                        "response_bytes",
                        MAX_WALK_RESPONSE_BYTES,
                    ));
                }
                continue;
            }
        };
        // Do not inspect ignore files beneath an unvalidated root link.
        if options.apply_gitignore && gitignore.is_none() {
            gitignore = Some(walk_gitignore::WalkGitignore::new(file_system, root, sandbox).await?);
        }
        let ignore_rules = match &mut gitignore {
            Some(gitignore) => {
                // A nested repository's own listing names its `.git` marker.
                let repository_root = batch
                    .entries
                    .iter()
                    .any(|entry| entry.sort_name().eq_ignore_ascii_case(".git"));
                // A complete listing without `.gitignore` needs no read; an
                // entry that failed enumeration may be that file.
                let ignore_file_possible = batch.limit_reached
                    || batch.entries.iter().any(|entry| {
                        matches!(entry, WalkDirectoryEntry::Error { .. })
                            || entry.sort_name().eq_ignore_ascii_case(".gitignore")
                    });
                let rules = gitignore
                    .enter_directory(
                        file_system,
                        &directory,
                        repository_root,
                        ignore_file_possible,
                        sandbox,
                    )
                    .await?;
                for (path, message) in gitignore.take_problems() {
                    if !push_walk_diagnostic(&mut outcome, &mut response_bytes, path, message) {
                        return Ok(finish_stopped_walk(
                            outcome,
                            root,
                            queue,
                            directory,
                            false,
                            "response_bytes",
                            MAX_WALK_RESPONSE_BYTES,
                        ));
                    }
                }
                rules
            }
            None => None,
        };
        entry_count += batch.entries_examined;
        if let Some(summary) = &mut outcome.summary {
            summary.entries_examined = entry_count;
            summary.directories_read += 1;
        }
        if batch.limit_reached {
            record_walk_stop(
                &mut outcome,
                root,
                directory.clone(),
                true,
                examination_reason,
                examination_limit,
            );
        }
        let mut entries = batch.entries;
        entries.sort_by(|left, right| left.sort_name().cmp(right.sort_name()));

        let no_follow = !options.follow_directory_symlinks;
        let mut probes = futures::stream::iter(entries)
            .map(|entry| {
                let directory = &directory;
                async move {
                    let (file_name, name_is_lossless, mut retained_metadata, native_path) =
                        match entry {
                            WalkDirectoryEntry::Entry {
                                file_name,
                                metadata,
                            } => (file_name, true, metadata, None),
                            WalkDirectoryEntry::NativeEntry(entry) => {
                                let NativeWalkDirectoryEntry {
                                    path,
                                    file_name,
                                    name_is_lossless,
                                    metadata,
                                } = *entry;
                                (file_name, name_is_lossless, metadata, Some(path))
                            }
                            WalkDirectoryEntry::Error { path, message } => {
                                return (String::new(), false, Ok((path, Err(io::Error::other(message)))));
                            }
                            WalkDirectoryEntry::UnrepresentableName { escaped_name } => {
                                return (
                                    String::new(),
                                    false,
                                    Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        format!(
                                            "unrepresentable native entry name: {escaped_name}"
                                        ),
                                    )),
                                );
                            }
                        };
                    let path = match native_path {
                        Some(path) => Ok(path),
                        None => directory.join(&file_name).map_err(|error| {
                            io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
                        }),
                    };
                    match path {
                        Ok(path) => {
                            let metadata = match retained_metadata.take() {
                                Some(metadata) => Ok(metadata),
                                None if no_follow => Err(io::Error::other("entry classification unavailable from no-follow enumeration; no target probe performed")),
                                None => file_system
                                    .get_metadata(&path, sandbox)
                                    .await
                                    .map(Into::into),
                            };
                            (file_name, name_is_lossless, Ok((path, metadata)))
                        }
                        Err(error) => (file_name, name_is_lossless, Err(error)),
                    }
                }
            })
            .buffered(MAX_CONCURRENT_WALK_METADATA);
        while let Some((file_name, name_is_lossless, result)) = probes.next().await {
            let (path, metadata) = match result {
                Ok(path) => path,
                Err(error) => {
                    if !push_walk_error(
                        &mut outcome,
                        &mut response_bytes,
                        directory.clone(),
                        error.to_string(),
                    ) {
                        return Ok(finish_stopped_walk(
                            outcome,
                            root,
                            queue,
                            directory.clone(),
                            true,
                            "response_bytes",
                            MAX_WALK_RESPONSE_BYTES,
                        ));
                    }
                    continue;
                }
            };
            let metadata = match metadata {
                Ok(metadata) => metadata,
                Err(error) => {
                    if !push_walk_error(&mut outcome, &mut response_bytes, path, error.to_string())
                    {
                        return Ok(finish_stopped_walk(
                            outcome,
                            root,
                            queue,
                            directory.clone(),
                            true,
                            "response_bytes",
                            MAX_WALK_RESPONSE_BYTES,
                        ));
                    }
                    continue;
                }
            };
            if metadata.is_symlink && metadata.is_directory && !options.follow_directory_symlinks {
                record_walk_skip(&mut outcome, "directory_symlink");
                continue;
            }

            let kind = if metadata.is_directory {
                WalkEntryKind::Directory
            } else if metadata.is_file {
                WalkEntryKind::File
            } else {
                record_walk_skip(&mut outcome, "non_file_or_directory");
                continue;
            };
            // File filters cannot prune directories needed for coverage. Only
            // reject a non-Unicode label when this entry actually needs a text
            // match; never apply a Unicode pattern to its replacement label.
            let needs_text_match = match kind {
                WalkEntryKind::File => {
                    options.filters.has_file_includes()
                        || !options.filters.exclude.is_empty()
                        || !options.filters.exclude_paths.is_empty()
                }
                WalkEntryKind::Directory => {
                    !options.filters.exclude_directories.is_empty()
                        || !options.filters.exclude_directory_paths.is_empty()
                }
            };
            let relative_path = relative_directory.as_ref().and_then(|parent| {
                name_is_lossless.then(|| {
                    if parent.is_empty() {
                        file_name.clone()
                    } else {
                        format!("{parent}/{file_name}")
                    }
                })
            });
            let needs_path_match = match kind {
                WalkEntryKind::File => {
                    !options.filters.include_paths.is_empty()
                        || !options.filters.exclude_paths.is_empty()
                }
                WalkEntryKind::Directory => !options.filters.exclude_directory_paths.is_empty(),
            };
            if (!name_is_lossless && (needs_text_match || options.apply_gitignore))
                || (needs_path_match && relative_path.is_none())
            {
                let message = format!("cannot apply text filters to native entry {path}");
                if !push_walk_error(&mut outcome, &mut response_bytes, path, message) {
                    return Ok(finish_stopped_walk(
                        outcome,
                        root,
                        queue,
                        directory.clone(),
                        true,
                        "response_bytes",
                        MAX_WALK_RESPONSE_BYTES,
                    ));
                }
                continue;
            }
            if ignore_rules
                .as_ref()
                .is_some_and(|rules| rules.is_ignored(&path, kind == WalkEntryKind::Directory))
            {
                outcome.gitignored_entries += 1;
                record_walk_skip(&mut outcome, "gitignored");
                continue;
            }
            if needs_text_match {
                name_chars.clear();
                name_chars.extend(file_name.chars());
                path_chars.clear();
                if needs_path_match && let Some(relative_path) = &relative_path {
                    path_chars.extend(relative_path.chars());
                }
                let path_segments = if needs_path_match {
                    path_chars.split(|c| *c == '/').collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                if (kind == WalkEntryKind::File
                    && !filters.includes_file(&name_chars, &path_segments))
                    || (kind == WalkEntryKind::Directory
                        && filters
                            .exclude_directories
                            .iter()
                            .any(|pattern| basename_matches(pattern, &name_chars)))
                    || (kind == WalkEntryKind::Directory
                        && filters
                            .exclude_directory_paths
                            .iter()
                            .any(|pattern| relative_path_matches(pattern, &path_segments)))
                {
                    record_walk_skip(&mut outcome, "pattern_filter");
                    continue;
                }
            }
            // Directories that include patterns do not return are still
            // traversed below.
            if kind == WalkEntryKind::File || !matching_files_only {
                // Misses never reach this budget. Stop at the first match it
                // cannot hold, leaving this directory partially examined.
                if matching_files_only && returned_entries == options.max_entries {
                    return Ok(finish_stopped_walk(
                        outcome,
                        root,
                        queue,
                        directory.clone(),
                        true,
                        "entry_limit",
                        options.max_entries,
                    ));
                }
                if let Some(summary) = &mut outcome.summary
                    && kind == WalkEntryKind::File
                {
                    summary.files += 1;
                    if let Some(details) = &metadata.details {
                        summary.file_bytes = summary
                            .file_bytes
                            .checked_add(details.size)
                            .ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "aggregate file byte total exceeds u64",
                                )
                            })?;
                    } else if !push_walk_diagnostic(
                        &mut outcome,
                        &mut response_bytes,
                        path.clone(),
                        "file size unavailable from enumeration; byte total is partial".into(),
                    ) {
                        return Ok(finish_stopped_walk(
                            outcome,
                            root,
                            queue,
                            directory.clone(),
                            true,
                            "response_bytes",
                            MAX_WALK_RESPONSE_BYTES,
                        ));
                    }
                }
                returned_entries += 1;
                let details = if options.include_metadata {
                    metadata.details
                } else {
                    None
                };
                let metadata_error = (options.include_metadata
                    && !options.summary_only
                    && details.is_none())
                .then(|| {
                    "entry metadata unavailable from enumeration; no additional probe performed"
                        .to_string()
                });
                if let Some(message) = &metadata_error
                    && !push_walk_diagnostic(
                        &mut outcome,
                        &mut response_bytes,
                        path.clone(),
                        message.clone(),
                    )
                {
                    return Ok(finish_stopped_walk(
                        outcome,
                        root,
                        queue,
                        directory.clone(),
                        true,
                        "response_bytes",
                        MAX_WALK_RESPONSE_BYTES,
                    ));
                }
                // Fixed numeric/bool fields and bounded diagnostic text. Include
                // their maximum serialized extent in the existing response budget.
                let metadata_bytes = if options.include_metadata { 320 } else { 0 };
                if !options.summary_only
                    && !reserve_walk_response_bytes(
                        &mut outcome,
                        &mut response_bytes,
                        path.as_str().len().saturating_add(metadata_bytes),
                    )
                {
                    return Ok(finish_stopped_walk(
                        outcome,
                        root,
                        queue,
                        directory.clone(),
                        true,
                        "response_bytes",
                        MAX_WALK_RESPONSE_BYTES,
                    ));
                }
                if !options.summary_only {
                    outcome.entries.push(WalkEntry {
                        path: path.clone(),
                        kind,
                        metadata: details,
                        metadata_error,
                    });
                }
            }

            if kind == WalkEntryKind::Directory {
                if let Some(summary) = &mut outcome.summary {
                    summary.directories += 1;
                }
                if options.prune_hidden_directories && file_name.starts_with('.') {
                    record_walk_skip(&mut outcome, "hidden_directory_subtree");
                    continue;
                }
                let directory_identity = if options.follow_directory_symlinks {
                    match file_system.canonicalize(&path, sandbox).await {
                        Ok(path) => path,
                        Err(error) => {
                            if !push_walk_error(
                                &mut outcome,
                                &mut response_bytes,
                                path,
                                error.to_string(),
                            ) {
                                return Ok(finish_stopped_walk(
                                    outcome,
                                    root,
                                    queue,
                                    directory.clone(),
                                    true,
                                    "response_bytes",
                                    MAX_WALK_RESPONSE_BYTES,
                                ));
                            }
                            continue;
                        }
                    }
                } else {
                    path.clone()
                };
                if !visited_directories.insert(directory_identity) {
                    record_walk_skip(&mut outcome, "duplicate_directory_subtree");
                    continue;
                }
                if depth == options.max_depth || directory_count == options.max_directories {
                    let (reason, limit) = if depth == options.max_depth {
                        ("depth_limit", options.max_depth)
                    } else {
                        ("directory_limit", options.max_directories)
                    };
                    record_walk_stop(&mut outcome, root, path, false, reason, limit);
                } else {
                    directory_count += 1;
                    queue.push_back((path, depth + 1, relative_path));
                }
            }
        }
    }

    Ok(outcome)
}

fn record_walk_skip(outcome: &mut WalkOutcome, reason: &str) {
    if let Some(summary) = &mut outcome.summary {
        *summary.skipped.entry(reason.into()).or_default() += 1;
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn record_walk_stop(
    outcome: &mut WalkOutcome,
    root: &PathUri,
    path: PathUri,
    partially_examined: bool,
    reason: &str,
    effective_limit: usize,
) {
    const MAX_FRONTIER: usize = 64;
    outcome.truncated = true;
    if outcome.unexplored.len() < MAX_FRONTIER {
        outcome.unexplored.push(WalkStop {
            path,
            reason: reason.into(),
            effective_limit,
            partially_examined,
        });
    } else {
        // A bounded conservative ancestor covers every omitted frontier item.
        // It explicitly requires a fresh walk, never a stable resume cursor.
        outcome.unexplored[MAX_FRONTIER - 1] = WalkStop {
            path: root.clone(),
            reason: "frontier_limit".into(),
            effective_limit: MAX_FRONTIER,
            partially_examined: true,
        };
    }
}

fn finish_stopped_walk(
    mut outcome: WalkOutcome,
    root: &PathUri,
    queue: VecDeque<(PathUri, usize, Option<String>)>,
    directory: PathUri,
    partially_examined: bool,
    reason: &str,
    limit: usize,
) -> WalkOutcome {
    record_walk_stop(
        &mut outcome,
        root,
        directory,
        partially_examined,
        reason,
        limit,
    );
    for (path, _, _) in queue {
        record_walk_stop(&mut outcome, root, path, false, reason, limit);
    }
    outcome
}

/// An entry or subtree that was skipped because it could not be inspected.
fn push_walk_error(
    outcome: &mut WalkOutcome,
    response_bytes: &mut usize,
    path: PathUri,
    message: String,
) -> bool {
    record_walk_skip(outcome, "inaccessible_or_invalid");
    push_walk_diagnostic(outcome, response_bytes, path, message)
}

/// A per-path error that skips no entry (e.g. one still counted or returned,
/// or an ignore file whose rules were not applied).
fn push_walk_diagnostic(
    outcome: &mut WalkOutcome,
    response_bytes: &mut usize,
    path: PathUri,
    message: String,
) -> bool {
    let item_bytes = path.as_str().len().saturating_add(message.len());
    if !reserve_walk_response_bytes(outcome, response_bytes, item_bytes) {
        return false;
    }
    outcome.errors.push(WalkError { path, message });
    true
}

fn reserve_walk_response_bytes(
    outcome: &mut WalkOutcome,
    response_bytes: &mut usize,
    content_bytes: usize,
) -> bool {
    let item_bytes = content_bytes.saturating_add(WALK_RESPONSE_ITEM_OVERHEAD_BYTES);
    let Some(total_bytes) = response_bytes.checked_add(item_bytes) else {
        outcome.truncated = true;
        return false;
    };
    if total_bytes > MAX_WALK_RESPONSE_BYTES {
        outcome.truncated = true;
        return false;
    }
    *response_bytes = total_bytes;
    true
}
pub use atomic_write::AtomicWriteLock;
pub use atomic_write::SymlinkWritePaths;
pub use atomic_write::acquire_atomic_write_lock;
pub use atomic_write::acquire_resolved_atomic_write_lock;
pub use atomic_write::atomic_write_lock_path;
pub use atomic_write::resolve_symlink_write_paths;
pub use atomic_write::try_acquire_atomic_write_lock;
pub use atomic_write::write_atomically;
pub use atomic_write::write_bytes_atomically;
pub use atomic_write::write_bytes_atomically_preserving_permissions;
pub use atomic_write::write_bytes_atomically_without_sync;
