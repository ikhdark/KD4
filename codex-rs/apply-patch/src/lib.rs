#[cfg(test)]
mod foreign_path_tests;
#[cfg(test)]
mod audit147_regressions;
#[cfg(test)]
mod verified_rebase_regressions;
mod invocation;
mod parser;
mod seek_sequence;
mod standalone_executable;
mod streaming_parser;
mod transaction;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FileMetadata;
use codex_exec_server::FileSystemSandboxContext;
#[cfg(test)]
use codex_exec_server::RemoveOptions;
use codex_utils_path_uri::PathUri;
use codex_utils_path_uri::PathUriParseError;
pub use parser::Hunk;
pub use parser::ParseError;
use parser::ParseError::*;
pub use parser::UpdateFileChunk;
pub use parser::parse_patch;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use similar::TextDiff;
pub use streaming_parser::StreamingPatchParser;
use thiserror::Error;

pub use invocation::apply_patch_command_cwd;
pub use invocation::apply_patch_command_scope;
pub use invocation::maybe_parse_apply_patch_verified;
pub use invocation::maybe_parse_apply_patch_verified_for_environment;
pub use invocation::verify_apply_patch_args;
pub use standalone_executable::main;
pub use standalone_executable::run_main_with_args;

use crate::invocation::ExtractHeredocError;

/// Special argv[1] flag used when the Codex executable self-invokes to run the
/// internal `apply_patch` path.
///
/// Although this constant lives in `codex-apply-patch` (to avoid forcing
/// `codex-arg0` to depend on `codex-core`), it remains part of the "codex core"
/// process-invocation contract for the standalone `apply_patch` command
/// surface.
pub const CODEX_CORE_APPLY_PATCH_ARG1: &str = "--codex-run-as-apply-patch";

#[derive(Debug, Error, PartialEq)]
pub enum ApplyPatchError {
    #[error(transparent)]
    ParseError(#[from] ParseError),
    #[error(transparent)]
    IoError(#[from] IoError),
    /// Error that occurs while computing replacements when applying patch chunks
    #[error("{0}")]
    ComputeReplacements(String),
    /// Preflight found several independent conflicts against one source
    /// snapshot. `report` is the same aggregated text a `ComputeReplacements`
    /// failure carried before, so model-facing output is unchanged; the typed
    /// mismatches are retained alongside it so a retry receipt can still name
    /// the observed source instead of forcing a separate re-read.
    #[error("{report}")]
    MultiplePatchConflicts {
        report: String,
        mismatches: Vec<PatchContextMismatch>,
    },
    /// A patch chunk did not match, with bounded context from the exact source
    /// snapshot observed by the confined patch runtime when safely available.
    #[error(transparent)]
    PatchContextMismatch(#[from] PatchContextMismatch),
    /// A patch path could not be resolved as a path URI.
    #[error(transparent)]
    PathUri(#[from] PathUriParseError),
    /// A raw patch body was provided without an explicit `apply_patch` invocation.
    #[error(
        "patch detected without explicit call to apply_patch. Send the raw patch body to the apply_patch tool, or invoke the shell executable as [\"apply_patch\", \"<patch>\"]"
    )]
    ImplicitInvocation,
    #[error(
        "patch environment id `{patch_environment_id}` does not match selected shell environment `{selected_environment_id}`; use the intended environment id from <environment_context> for both"
    )]
    EnvironmentIdMismatch {
        patch_environment_id: String,
        selected_environment_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchContextMismatchKind {
    ContextNotFound,
    ExpectedLinesNotFound,
    AmbiguousMatch,
    IndentationMismatch,
    RemovedLineMismatch,
    MalformedRange,
    RangeOutOfBounds,
    RangeOrderMismatch,
    StaleRange,
    RangeOldLinesMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PatchContextMismatch {
    pub kind: PatchContextMismatchKind,
    pub hunk_ordinal: usize,
    pub chunk_ordinal: usize,
    pub canonical_path: String,
    pub current_content_sha256: String,
    /// Zero when no safe diagnostic excerpt is available.
    pub current_line_start: usize,
    pub current_line_end: usize,
    pub current_excerpt: String,
    pub message: String,
    /// The earlier hunk for the same file whose in-memory result this hunk
    /// edits; the current lines and hash then describe that result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edits_result_of_hunk: Option<usize>,
}

impl fmt::Display for PatchContextMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.current_line_start == 0 {
            return write!(
                formatter,
                "PatchContextMismatch: {}\nFile: {}\nHunk {}, chunk {} (sha256: {}). Current excerpt unavailable; read the target region before retrying.",
                self.message,
                self.canonical_path,
                self.hunk_ordinal,
                self.chunk_ordinal,
                self.current_content_sha256
            );
        }
        write!(
            formatter,
            "PatchContextMismatch: {}\nFile: {}\nHunk {}, chunk {}. Current lines {}-{} (sha256: {}):\n{}",
            self.message,
            self.canonical_path,
            self.hunk_ordinal,
            self.chunk_ordinal,
            self.current_line_start,
            self.current_line_end,
            self.current_content_sha256,
            self.current_excerpt,
        )
    }
}

impl std::error::Error for PatchContextMismatch {}

impl From<std::io::Error> for ApplyPatchError {
    fn from(err: std::io::Error) -> Self {
        ApplyPatchError::IoError(IoError {
            context: "I/O error".to_string(),
            source: err,
        })
    }
}

impl From<&std::io::Error> for ApplyPatchError {
    fn from(err: &std::io::Error) -> Self {
        ApplyPatchError::IoError(IoError {
            context: "I/O error".to_string(),
            source: std::io::Error::new(err.kind(), err.to_string()),
        })
    }
}

#[derive(Debug, Error)]
#[error("{context}: {source}")]
pub struct IoError {
    context: String,
    #[source]
    source: std::io::Error,
}

impl PartialEq for IoError {
    fn eq(&self, other: &Self) -> bool {
        self.context == other.context && self.source.to_string() == other.source.to_string()
    }
}

/// Both the raw PATCH argument to `apply_patch` as well as the PATCH argument
/// parsed into hunks.
#[derive(Debug, PartialEq)]
pub struct ApplyPatchArgs {
    pub patch: String,
    pub hunks: Vec<Hunk>,
    pub workdir: Option<String>,
    pub environment_id: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum ApplyPatchFileChange {
    Add {
        content: String,
    },
    Delete {
        content: String,
    },
    Update {
        unified_diff: String,
        move_path: Option<PathUri>,
        /// new_content that will result after the unified_diff is applied.
        new_content: String,
    },
}

#[derive(Debug, PartialEq)]
pub enum MaybeApplyPatchVerified {
    /// `argv` corresponded to an `apply_patch` invocation, and these are the
    /// resulting proposed file changes.
    Body(ApplyPatchAction),
    /// `argv` could not be parsed to determine whether it corresponds to an
    /// `apply_patch` invocation.
    ShellParseError(ExtractHeredocError),
    /// `argv` corresponded to an `apply_patch` invocation, but it could not
    /// be fulfilled due to the specified error.
    CorrectnessError(ApplyPatchError),
    /// `argv` decidedly did not correspond to an `apply_patch` invocation.
    NotApplyPatch,
}

/// ApplyPatchAction is the result of parsing an `apply_patch` command. By
/// construction, all paths should be absolute paths.
#[derive(Debug, PartialEq)]
pub struct ApplyPatchAction {
    changes: HashMap<PathUri, ApplyPatchFileChange>,
    destination_preimages: HashMap<PathUri, Option<String>>,

    /// The raw patch argument that can be used to apply the patch. i.e., if the
    /// original arg was parsed in "lenient" mode with a
    /// heredoc, this should be the value without the heredoc wrapper.
    pub patch: String,

    /// The working directory that was used to resolve relative paths in the patch.
    pub cwd: PathUri,
}

impl ApplyPatchAction {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Returns the changes that would be made by applying the patch.
    pub fn changes(&self) -> &HashMap<PathUri, ApplyPatchFileChange> {
        &self.changes
    }

    /// Content an Add target or move destination had at verification, or
    /// `None` when it was absent. Application refuses to proceed if it changed,
    /// so a projection may show it as the content the patch displaces.
    pub fn destination_preimage(&self, path: &PathUri) -> Option<&str> {
        self.destination_preimages.get(path).and_then(Option::as_deref)
    }

    /// Should be used exclusively for testing. (Not worth the overhead of
    /// creating a feature flag for this.)
    pub fn new_add_for_test(path: &PathUri, content: String) -> Self {
        #[expect(clippy::expect_used)]
        let filename = path.basename().expect("path should not be empty");
        // Add hunks terminate every logical line with a newline.
        let mut content = content;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        let mut patch = format!("*** Begin Patch\n*** Add File: {filename}\n");
        for line in content.split_terminator('\n') {
            patch.push('+');
            patch.push_str(line);
            if line.ends_with('\r') {
                patch.push('\r');
            }
            patch.push('\n');
        }
        patch.push_str("*** End Patch");
        let changes = HashMap::from([(path.clone(), ApplyPatchFileChange::Add { content })]);
        #[expect(clippy::expect_used)]
        Self {
            changes,
            destination_preimages: HashMap::from([(path.clone(), None)]),
            cwd: path.parent().expect("path should have parent"),
            patch,
        }
    }
}

/// Textual file changes that were actually committed while applying a patch.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedPatchDelta {
    changes: Vec<AppliedPatchChange>,
    exact: bool,
    // Captured before mutation, including deletes; never reconstructed later.
    endpoint_identities: HashMap<PathBuf, String>,
    preimage_modes: HashMap<PathBuf, String>,
}

impl AppliedPatchDelta {
    fn new(changes: Vec<AppliedPatchChange>, exact: bool) -> Self {
        Self { changes, exact, endpoint_identities: HashMap::new(), preimage_modes: HashMap::new() }
    }

    fn empty() -> Self {
        Self::new(Vec::new(), /*exact*/ true)
    }

    /// An operation may have changed the filesystem without identifying any
    /// committed text change. An empty list is not proof of no mutation.
    pub fn unknown() -> Self {
        Self::new(Vec::new(), /*exact*/ false)
    }

    pub fn changes(&self) -> &[AppliedPatchChange] {
        &self.changes
    }

    /// Continuity paths only. Original spellings and executor URIs remain in
    /// `changes()` for display, recovery and mutation authority.
    pub fn endpoint_change(&self, change: &AppliedPatchChange) -> AppliedPatchChange {
        let endpoint = |path: &PathBuf| self.endpoint_identities.get(path)
            .and_then(|identity| PathUri::parse(identity).ok())
            .map_or_else(|| path.clone(), |uri| uri.to_path_buf());
        let mut result = change.clone();
        result.path_uri = self.endpoint_identities.get(&change.path)
            .and_then(|identity| PathUri::parse(identity).ok()).unwrap_or_else(|| change.path_uri.clone());
        result.path = endpoint(&change.path);
        if let AppliedPatchFileChange::Update { move_path: Some(path), .. } = &mut result.change {
            *path = endpoint(path);
            result.move_path_uri = change.move_path_uri.as_ref().map(|uri| {
                self.endpoint_identities.get(&uri.to_path_buf()).and_then(|identity| PathUri::parse(identity).ok())
                    .unwrap_or_else(|| uri.clone())
            });
        }
        result
    }

    pub fn retained_endpoint_bytes(&self) -> usize {
        self.endpoint_identities.iter().map(|(path, identity)| path.as_os_str().len().saturating_add(identity.len())).fold(0usize, usize::saturating_add)
    }

    pub fn preimage_modes(&self) -> &HashMap<PathBuf, String> {
        &self.preimage_modes
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn is_exact(&self) -> bool {
        self.exact
    }

    /// Recovery guidance for a failed or cancelled operation, including any
    /// earlier attempts whose changes have been appended to this delta.
    pub fn failure_summary(&self) -> String {
        if self.is_empty() && self.is_exact() {
            return String::new();
        }
        let mut summary = String::from("Patch failed after applying these changes:\n");
        for applied in self.changes() {
            let path = applied.path.display();
            match &applied.change {
                AppliedPatchFileChange::Add { .. } => {
                    let _ = writeln!(summary, "A {path}");
                }
                AppliedPatchFileChange::Delete { .. } => {
                    let _ = writeln!(summary, "D {path}");
                }
                AppliedPatchFileChange::Update { move_path, .. } => {
                    if let Some(destination) = move_path {
                        let _ = writeln!(summary, "M {path} -> {}", destination.display());
                    } else {
                        let _ = writeln!(summary, "M {path}");
                    }
                }
            }
        }
        if !self.is_exact() {
            summary.push_str("Additional filesystem changes may not be listed.\n");
        }
        summary.push_str(
            "Inspect the current files before retrying the remaining changes; do not retry the whole patch.\n",
        );
        summary
    }

    /// Appends a later committed prefix while preserving the aggregate exactness.
    pub fn append(&mut self, other: Self) {
        for (path, mode) in other.preimage_modes {
            self.preimage_modes.entry(path).or_insert(mode);
        }
        for (path, identity) in other.endpoint_identities {
            if self.endpoint_identities.get(&path).is_some_and(|old| old != &identity) {
                self.exact = false;
            }
            self.endpoint_identities.insert(path, identity);
        }
        self.changes.extend(other.changes);
        self.exact &= other.exact;
    }
}

impl Default for AppliedPatchDelta {
    fn default() -> Self {
        Self::empty()
    }
}

/// A committed file change, preserved in the order it was applied.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedPatchChange {
    pub path: PathBuf,
    pub change: AppliedPatchFileChange,
    // The public paths are legacy presentation values, not executor authority.
    path_uri: PathUri,
    move_path_uri: Option<PathUri>,
}

impl AppliedPatchChange {
    pub fn path_uri(&self) -> &PathUri { &self.path_uri }
    pub fn move_path_uri(&self) -> Option<&PathUri> { self.move_path_uri.as_ref() }

    /// Bind retry exclusion to operation semantics and bytes, not path alone.
    pub fn matches_hunk(&self, hunk: &Hunk, cwd: &PathUri) -> bool {
        if hunk.resolve_path(cwd).ok().as_ref() != Some(&self.path_uri) { return false; }
        match (hunk, &self.change) {
            (Hunk::AddFile { contents, .. }, AppliedPatchFileChange::Add { content, overwritten_content: None }) => contents == content,
            (Hunk::DeleteFile { .. }, AppliedPatchFileChange::Delete { .. }) => true,
            (Hunk::UpdateFile { move_path, chunks, .. }, AppliedPatchFileChange::Update { old_content, new_content, .. }) => {
                move_path.as_ref().map(|path| cwd.join(&path.to_string_lossy())).transpose().ok()
                    == Some(self.move_path_uri.clone())
                    && new_contents_from_chunks(&self.path_uri, old_content, chunks, None).ok().as_ref() == Some(new_content)
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AppliedPatchFileChange {
    Add {
        content: String,
        overwritten_content: Option<String>,
    },
    Delete {
        content: String,
    },
    Update {
        move_path: Option<PathBuf>,
        old_content: String,
        overwritten_move_content: Option<String>,
        new_content: String,
    },
}

/// A failed patch application together with the textual mutations that were
/// definitely committed before the failure was observed.
#[derive(Debug, Error)]
#[error("{error}")]
pub struct ApplyPatchFailure {
    #[source]
    error: ApplyPatchError,
    delta: AppliedPatchDelta,
}

impl ApplyPatchFailure {
    fn new(error: ApplyPatchError, delta: AppliedPatchDelta) -> Self {
        Self { error, delta }
    }

    fn without_delta(error: ApplyPatchError) -> Self {
        Self::new(error, AppliedPatchDelta::empty())
    }

    pub fn delta(&self) -> &AppliedPatchDelta {
        &self.delta
    }

    pub fn into_parts(self) -> (ApplyPatchError, AppliedPatchDelta) {
        (self.error, self.delta)
    }
}

/// Applies the patch and prints the result to stdout/stderr.
pub async fn apply_patch(
    patch: &str,
    cwd: &PathUri,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> Result<AppliedPatchDelta, ApplyPatchFailure> {
    apply_patch_with_cancellation(patch, cwd, stdout, stderr, fs, sandbox, &|| false).await
}

/// Applies a patch, finishing each started hunk before observing cancellation.
/// On failure, restore known originals and report any changes still installed.
pub async fn apply_patch_with_cancellation(
    patch: &str,
    cwd: &PathUri,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    is_cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<AppliedPatchDelta, ApplyPatchFailure> {
    let hunks = match parse_patch(patch) {
        Ok(source) => source.hunks,
        Err(e) => {
            match &e {
                InvalidPatchError(message) => {
                    writeln!(stderr, "Invalid patch: {message}")
                        .map_err(ApplyPatchError::from)
                        .map_err(ApplyPatchFailure::without_delta)?;
                }
                InvalidHunkError {
                    message,
                    line_number,
                } => {
                    writeln!(
                        stderr,
                        "Invalid patch hunk on line {line_number}: {message}"
                    )
                    .map_err(ApplyPatchError::from)
                    .map_err(ApplyPatchFailure::without_delta)?;
                }
            }
            return Err(ApplyPatchFailure::without_delta(
                ApplyPatchError::ParseError(e),
            ));
        }
    };

    apply_hunks_with_cancellation(&hunks, cwd, stdout, stderr, fs, sandbox, is_cancelled).await
}

/// Applies hunks and continues to update stdout/stderr
pub async fn apply_hunks(
    hunks: &[Hunk],
    cwd: &PathUri,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> Result<AppliedPatchDelta, ApplyPatchFailure> {
    apply_hunks_with_cancellation(hunks, cwd, stdout, stderr, fs, sandbox, &|| false).await
}

async fn apply_hunks_with_cancellation(
    hunks: &[Hunk],
    cwd: &PathUri,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    is_cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<AppliedPatchDelta, ApplyPatchFailure> {
    apply_hunks_with_verified_action(hunks, cwd, stdout, stderr, fs, sandbox, is_cancelled, None).await
}

/// Execute the verified destructive preimages, rather than selecting a newer
/// delete/destination baseline when approval or scheduling took time.
pub async fn apply_verified_patch_with_cancellation(
    action: &ApplyPatchAction,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    is_cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<AppliedPatchDelta, ApplyPatchFailure> {
    let args = parse_patch(&action.patch)
        .map_err(|error| ApplyPatchFailure::without_delta(error.into()))?;
    apply_hunks_with_verified_action(
        &args.hunks, &action.cwd, stdout, stderr, fs, sandbox, is_cancelled, Some(action),
    ).await
}

async fn apply_hunks_with_verified_action(
    hunks: &[Hunk],
    cwd: &PathUri,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    is_cancelled: &(dyn Fn() -> bool + Sync),
    verified: Option<&ApplyPatchAction>,
) -> Result<AppliedPatchDelta, ApplyPatchFailure> {
    let mut delta = AppliedPatchDelta::empty();
    // Keep endpoint ownership alive through failure reconciliation and rollback,
    // not just the successful write path. Preflight has no filesystem effects.
    let mut _transaction = None;
    let result = async {
        if hunks.is_empty() {
            anyhow::bail!("No files were modified.");
        }
        let previous_updates =
            invocation::validate_mutation_endpoints(hunks, cwd, fs, sandbox).await?;
        _transaction = transaction::acquire(hunks, cwd, fs, sandbox).await?;
        let destinations = capture_destination_preimages(hunks, cwd, fs, sandbox).await?;
        if let Some(action) = verified {
            anyhow::ensure!(destinations == action.destination_preimages,
                "patch destination changed after verification; no files were changed");
        }
        let prepared = preflight_hunks(hunks, &previous_updates, cwd, fs, sandbox).await?;
        if let Some(action) = verified {
            for (path, change) in action.changes() {
                if let ApplyPatchFileChange::Delete { content } = change {
                    anyhow::ensure!(fs.read_file_text(path, sandbox).await? == *content,
                        "delete source changed after verification; refusing to delete {}", path);
                    let index = hunks.iter().position(|hunk| hunk.resolve_path(cwd).ok().as_ref() == Some(path))
                        .context("verified delete is missing from patch")?;
                    anyhow::ensure!(prepared[index].as_ref().is_some_and(|source| source.original_contents == *content),
                        "delete source changed during preparation; refusing to delete {}", path);
                }
            }
            ensure_rebased_updates_keep_their_sites(hunks, cwd, &previous_updates, &prepared, action)?;
        }
        apply_hunks_to_files(hunks, cwd, fs, sandbox, &mut delta, prepared, &destinations, is_cancelled).await
    }
    .await;
    match result {
        Ok(()) => {
            if let Err(error) = print_summary(hunks, stdout) {
                let _ = write!(stderr, "{error}\n{}", delta.failure_summary());
                return Err(ApplyPatchFailure::new(ApplyPatchError::from(error), delta));
            }
            Ok(delta)
        }
        Err(error) => {
            let recovery_path = error
                .downcast_ref::<PatchRecoveryRequired>()
                .map(|recovery| &recovery.path);
            let rollback_errors =
                rollback_committed_changes(&mut delta, fs, sandbox, recovery_path).await;
            let error = if rollback_errors.is_empty() {
                error
            } else {
                error.context(format!(
                    "rollback incomplete: {}",
                    rollback_errors.join("; ")
                ))
            };
            let msg = format!("{error:#}");
            writeln!(stderr, "{msg}").map_err(|error| {
                ApplyPatchFailure::new(ApplyPatchError::from(error), delta.clone())
            })?;
            write!(stderr, "{}", delta.failure_summary()).map_err(|error| {
                ApplyPatchFailure::new(ApplyPatchError::from(error), delta.clone())
            })?;
            let error = match error.downcast::<ApplyPatchError>() {
                Ok(error) => error,
                Err(error) => match error.downcast::<std::io::Error>() {
                    Ok(source) => ApplyPatchError::IoError(IoError {
                        context: msg,
                        source,
                    }),
                    Err(error) => ApplyPatchError::IoError(IoError {
                        context: msg,
                        source: std::io::Error::other(error),
                    }),
                },
            };
            Err(ApplyPatchFailure::new(error, delta))
        }
    }
}

/// Undo only bytes still owned by this patch. Do not overwrite an intervening
/// edit or guess at originals that could not be captured as text.
async fn rollback_committed_changes(
    delta: &mut AppliedPatchDelta,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    recovery_path: Option<&PathUri>,
) -> Vec<String> {
    let mut errors = Vec::new();
    // Split moves into their actual writes so a partially restored move still
    // has an exact delta (e.g. source restored, destination remains installed).
    delta.changes = std::mem::take(&mut delta.changes)
        .into_iter()
        .flat_map(|applied| {
            match (applied.change, applied.move_path_uri) {
                (
                    AppliedPatchFileChange::Update {
                        move_path: Some(destination),
                        old_content,
                        overwritten_move_content,
                        new_content,
                    },
                    Some(destination_uri),
                ) => vec![
                    AppliedPatchChange {
                        path: destination,
                        path_uri: destination_uri,
                        move_path_uri: None,
                        change: AppliedPatchFileChange::Add {
                            content: new_content,
                            overwritten_content: overwritten_move_content,
                        },
                    },
                    AppliedPatchChange {
                        path: applied.path,
                        path_uri: applied.path_uri,
                        move_path_uri: None,
                        change: AppliedPatchFileChange::Delete {
                            content: old_content,
                        },
                    },
                ],
                // Committed moves record their destination URI. Without it the
                // destination cannot be restored exactly: keep the move whole and
                // report the delta as inexact instead of guessing a location.
                (
                    change @ AppliedPatchFileChange::Update {
                        move_path: Some(_), ..
                    },
                    None,
                ) => {
                    delta.exact = false;
                    vec![AppliedPatchChange {
                        path: applied.path,
                        path_uri: applied.path_uri,
                        move_path_uri: None,
                        change,
                    }]
                }
                (change, move_path_uri) => vec![AppliedPatchChange {
                    path: applied.path,
                    path_uri: applied.path_uri,
                    move_path_uri,
                    change,
                }],
            }
        })
        .collect();
    for index in (0..delta.changes.len()).rev() {
        let applied = &delta.changes[index];
        let path = &applied.path_uri;
        if recovery_path == Some(path) {
            // A failed suffix may have published these same bytes but left
            // displaced-object recovery unfinished. Equality cannot authorize
            // rolling that endpoint back as part of the earlier prefix.
            delta.exact = false;
            continue;
        }
        let (expected, original) = match &applied.change {
            AppliedPatchFileChange::Add {
                content,
                overwritten_content,
            } => (Some(content.as_str()), overwritten_content.as_deref()),
            AppliedPatchFileChange::Delete { content } => (None, Some(content.as_str())),
            // Only a move kept whole above remains unsplit. Its source no longer
            // holds this patch's bytes, so leave the recorded move untouched.
            AppliedPatchFileChange::Update {
                move_path: Some(_), ..
            } => continue,
            AppliedPatchFileChange::Update {
                old_content,
                new_content,
                ..
            } => (Some(new_content.as_str()), Some(old_content.as_str())),
        };
        let regular_or_absent = match fs.get_metadata(path, sandbox).await {
            Ok(metadata) => metadata.is_file && !metadata.is_symlink,
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        };
        if !regular_or_absent
            || !remove_failure_was_side_effect_free(path, expected, fs, sandbox).await
        {
            delta.exact = false;
            continue;
        }
        let restored = conditional_mutation(
            fs,
            path,
            expected,
            original.map(|content| content.as_bytes().to_vec()),
            sandbox,
            &mut delta.exact,
        )
        .await;
        match restored {
            Ok(()) => {
                delta.changes.remove(index);
            }
            Err(error) => {
                // Keep any actual displaced-object recovery locator visible
                // instead of discarding the rollback error behind the first failure.
                errors.push(format!("{error:#}"));
                delta.exact &=
                    remove_failure_was_side_effect_free(path, expected, fs, sandbox).await;
            }
        }
    }
    errors
}

/// Apply the hunks to the filesystem, recording committed changes in the delta.
/// Returns an error if the patch could not be applied.
async fn apply_hunks_to_files(
    hunks: &[Hunk],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    delta: &mut AppliedPatchDelta,
    mut prepared: Vec<Option<AppliedPatch>>,
    destinations: &HashMap<PathUri, Option<String>>,
    is_cancelled: &(dyn Fn() -> bool + Sync),
) -> anyhow::Result<()> {
    let previous_updates =
        invocation::validate_mutation_endpoints(hunks, cwd, fs, sandbox).await?;

    anyhow::ensure!(capture_destination_preimages(hunks, cwd, fs, sandbox).await? == *destinations,
        "patch destination changed during preparation; no files were changed");

    // Verify the complete read set before the first write. Local endpoint leases
    // exclude participating patch processes through rollback. Per-file checks
    // detect observed external edits, not CAS against nonparticipating writers.
    // I/O failures still report the exact committed delta.
    for (index, hunk) in hunks.iter().enumerate() {
        let Some(previous) = &prepared[index] else {
            continue;
        };
        let path = hunk.resolve_path(cwd)?;
        // A repeated update edits its predecessor's result, which exists only
        // in memory until the predecessor is written.
        let previous_update = previous_updates.get(index).copied().flatten();
        let source = match previous_update {
            Some(previous_update) => {
                let Some(update) = &prepared[previous_update] else {
                    anyhow::bail!(
                        "update was not prepared before writes: {}",
                        path.inferred_native_path_string()
                    );
                };
                update.new_contents.clone()
            }
            None => fs.read_file_text(&path, sandbox).await?,
        };
        if source == previous.original_contents {
            continue;
        }
        let Hunk::UpdateFile { chunks, .. } = hunk else {
            anyhow::bail!(
                "source changed during patch preparation: {}",
                path.inferred_native_path_string()
            );
        };
        let new_contents = new_contents_from_chunks(&path, &source, chunks, Some(index + 1))
            .map_err(|mut error| {
                mark_chained_mismatch(&mut error, previous_update);
                error
            })?;
        let update = AppliedPatch {
            new_contents,
            original_contents: source,
        };
        // Rebuilding against a newer snapshot must enforce the same guard as
        // preflight.
        if let Some(error) =
            update_json_regression(hunk.path(), index, &previous_updates, &prepared, &update)
        {
            return Err(ApplyPatchError::ComputeReplacements(format!(
                "{} was valid JSON before this patch; candidate is invalid: {error}",
                path.inferred_native_path_string(),
            ))
            .into());
        }
        prepared[index] = Some(update);
    }

    // A failed write may truncate its destination, but failures such as Windows
    // sharing violations leave it unchanged. Reconcile the known source before
    // discarding the exact committed prefix; never infer no mutation from the
    // error kind alone.
    macro_rules! try_write {
        ($result:expr, $path:expr, $before:expr) => {
            match $result {
                Ok(value) => value,
                Err(error) => {
                    if !error.is::<PatchNoWriteConflict>() {
                        delta.exact &= remove_failure_was_side_effect_free($path, $before, fs, sandbox).await;
                    }
                    return Err(anyhow::Error::from(error));
                }
            }
        };
    }

    // Keep executor URIs for recovery; legacy paths are only for diff/event consumers.
    for (hunk_index, hunk) in hunks.iter().enumerate() {
        if is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Patch application cancelled before the next hunk",
            )
            .into());
        }
        let path_uri = hunk.resolve_path(cwd)?;
        if let Some(mode) = preimage_mode(&path_uri, fs).await {
            delta.preimage_modes.entry(path_uri.to_path_buf()).or_insert(mode);
        }
        let identity = invocation::mutation_endpoint_identity(fs, &path_uri, sandbox).await?;
        let previous = delta.endpoint_identities.insert(path_uri.to_path_buf(), identity);
        if previous.as_ref().is_some_and(|previous| delta.endpoint_identities.get(&path_uri.to_path_buf()) != Some(previous)) {
            delta.exact = false;
        }
        match hunk {
            Hunk::AddFile { contents, .. } => {
                let overwritten_content = destinations.get(&path_uri)
                    .context("addition destination was not prepared")?.clone();
                anyhow::ensure!(
                    overwritten_content.is_none(),
                    "Add File is create-only; {} already exists. Use Update File with a current codex-range source hash for a whole-file replacement.",
                    path_uri.inferred_native_path_string(),
                );
                try_write!(
                    write_file_with_missing_parent_retry(
                        fs,
                        &path_uri,
                        overwritten_content.as_deref(),
                        contents.clone().into_bytes(),
                        sandbox,
                        &mut delta.exact,
                    )
                    .await,
                    &path_uri,
                    overwritten_content.as_deref()
                );
                if let Some(mode) = preimage_mode(&path_uri, fs).await {
                    delta.preimage_modes.entry(path_uri.to_path_buf()).or_insert(mode);
                }
                delta.changes.push(AppliedPatchChange {
                    path: path_uri.to_path_buf(),
                    path_uri,
                    move_path_uri: None,
                    change: AppliedPatchFileChange::Add {
                        content: contents.clone(),
                        overwritten_content,
                    },
                });
            }
            Hunk::DeleteFile { .. } => {
                let metadata = ensure_not_directory(&path_uri, fs, sandbox)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to delete file {}",
                            path_uri.inferred_native_path_string()
                        )
                    })?;
                delta.exact &= metadata.is_file && !metadata.is_symlink;
                let deleted_content = fs.read_file_text(&path_uri, sandbox).await?;
                anyhow::ensure!(
                    prepared[hunk_index]
                        .as_ref()
                        .is_some_and(|source| source.original_contents == deleted_content),
                    "source changed after patch preparation; refusing to delete {}",
                    path_uri.inferred_native_path_string()
                );
                if let Err(error) = conditional_mutation(
                    fs,
                    &path_uri,
                    Some(&deleted_content),
                    None,
                    sandbox,
                    &mut delta.exact,
                )
                .await
                .with_context(|| {
                    format!(
                        "Failed to delete file {}",
                        path_uri.inferred_native_path_string()
                    )
                }) {
                    if !error.is::<PatchNoWriteConflict>() {
                        delta.exact &= remove_failure_was_side_effect_free(
                        &path_uri,
                        Some(&deleted_content),
                        fs,
                        sandbox,
                        ).await;
                    }
                    return Err(error);
                }
                delta.changes.push(AppliedPatchChange {
                    path: path_uri.to_path_buf(),
                    path_uri,
                    move_path_uri: None,
                    change: AppliedPatchFileChange::Delete {
                        content: deleted_content,
                    },
                });
            }
            Hunk::UpdateFile {
                move_path, chunks, ..
            } => {
                note_existing_path_delta_support(&path_uri, fs, sandbox, &mut delta.exact).await;
                let Some(mut update) = prepared[hunk_index].take() else {
                    anyhow::bail!(
                        "update was not prepared before writes: {}",
                        path_uri.inferred_native_path_string()
                    );
                };
                // This comparison read is the pre-write source check. Only a
                // re-derived update needs another read to confirm its snapshot.
                if fs.read_file_text(&path_uri, sandbox).await? != update.original_contents {
                    update = derive_new_contents_from_chunks(
                        &path_uri,
                        chunks,
                        Some(hunk_index + 1),
                        fs,
                        sandbox,
                    )
                    .await?;
                    anyhow::ensure!(
                        fs.read_file_text(&path_uri, sandbox).await? == update.original_contents,
                        "source changed after patch preparation: {}",
                        path_uri.inferred_native_path_string()
                    );
                }
                let AppliedPatch {
                    original_contents,
                    new_contents,
                } = update;
                if let Some(dest) = move_path {
                    let dest_uri = cwd.join(&dest.to_string_lossy())?;
                    if let Some(mode) = preimage_mode(&dest_uri, fs).await {
                        delta.preimage_modes.entry(dest_uri.to_path_buf()).or_insert(mode);
                    }
                    let destination_identity = invocation::mutation_endpoint_identity(fs, &dest_uri, sandbox).await?;
                    delta.endpoint_identities.insert(dest_uri.to_path_buf(), destination_identity);
                    let source_identity =
                        invocation::mutation_endpoint_identity(fs, &path_uri, sandbox).await?;
                    if source_identity
                        == invocation::mutation_endpoint_identity(fs, &dest_uri, sandbox).await?
                    {
                        return Err(ApplyPatchError::ParseError(InvalidPatchError(format!(
                            "move source and destination identify the same path: {}",
                            path_uri.inferred_native_path_string()
                        )))
                        .into());
                    }
                    note_existing_path_delta_support(&dest_uri, fs, sandbox, &mut delta.exact).await;
                    let overwritten_move_content = destinations.get(&dest_uri)
                        .context("move destination was not prepared")?.clone();
                    try_write!(
                        write_file_with_missing_parent_retry(
                            fs,
                            &dest_uri,
                            overwritten_move_content.as_deref(),
                            new_contents.clone().into_bytes(),
                            sandbox,
                            &mut delta.exact,
                        )
                        .await,
                        &dest_uri,
                        overwritten_move_content.as_deref()
                    );
                    if let Some(mode) = preimage_mode(&dest_uri, fs).await {
                        delta.preimage_modes.entry(dest_uri.to_path_buf()).or_insert(mode);
                    }
                    let dest_write_change_index = delta.changes.len();
                    delta.changes.push(AppliedPatchChange {
                        path: dest_uri.to_path_buf(),
                        path_uri: dest_uri.clone(),
                        move_path_uri: None,
                        change: AppliedPatchFileChange::Add {
                            content: new_contents.clone(),
                            overwritten_content: overwritten_move_content.clone(),
                        },
                    });
                    if is_cancelled() {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "Patch application cancelled after move destination publication",
                        )
                        .into());
                    }
                    ensure_not_directory(&path_uri, fs, sandbox)
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to remove original {}",
                                path_uri.inferred_native_path_string()
                            )
                        })?;
                    anyhow::ensure!(
                        invocation::mutation_endpoint_identity(fs, &path_uri, sandbox).await?
                            == source_identity
                            && fs.read_file_text(&path_uri, sandbox).await? == original_contents,
                        "move source changed during destination publication; refusing to remove {}",
                        path_uri.inferred_native_path_string()
                    );
                    if let Err(error) = conditional_mutation(
                        fs,
                        &path_uri,
                        Some(&original_contents),
                        None,
                        sandbox,
                        &mut delta.exact,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to remove original {}",
                            path_uri.inferred_native_path_string()
                        )
                    }) {
                        if !error.is::<PatchNoWriteConflict>() {
                            delta.exact &= remove_failure_was_side_effect_free(
                            &path_uri,
                            Some(&original_contents),
                            fs,
                            sandbox,
                            ).await;
                        }
                        return Err(error);
                    }
                    delta.changes[dest_write_change_index] = AppliedPatchChange {
                        path: path_uri.to_path_buf(),
                        path_uri,
                        move_path_uri: Some(dest_uri.clone()),
                        change: AppliedPatchFileChange::Update {
                            move_path: Some(dest_uri.to_path_buf()),
                            old_content: original_contents,
                            overwritten_move_content,
                            new_content: new_contents,
                        },
                    };
                } else {
                    // An update requires the source to remain present; unlike add/move,
                    // it must not recreate a parent removed after the source read.
                    try_write!(
                        conditional_mutation(
                            fs,
                            &path_uri,
                            Some(&original_contents),
                            Some(new_contents.clone().into_bytes()),
                            sandbox,
                            &mut delta.exact
                        )
                        .await
                        .with_context(|| format!(
                            "Failed to write file {}",
                            path_uri.inferred_native_path_string()
                        )),
                        &path_uri,
                        Some(original_contents.as_str())
                    );
                    delta.changes.push(AppliedPatchChange {
                        path: path_uri.to_path_buf(),
                        path_uri,
                        move_path_uri: None,
                        change: AppliedPatchFileChange::Update {
                            move_path: None,
                            old_content: original_contents,
                            overwritten_move_content: None,
                            new_content: new_contents,
                        },
                    });
                }
            }
        }
    }
    Ok(())
}

/// Prepare every update and report every independently invalid chunk in one
/// response. A successful result is reusable only against these exact bytes.
pub(crate) async fn preflight_hunks(
    hunks: &[Hunk],
    previous_updates: &[Option<usize>],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<Vec<Option<AppliedPatch>>, ApplyPatchError> {
    let mut prepared = Vec::with_capacity(hunks.len());
    let mut failures = Vec::new();
    for (ordinal, hunk) in hunks.iter().enumerate() {
        let path = hunk.resolve_path(cwd)?;
        match hunk {
            Hunk::UpdateFile { chunks, .. } => {
                let hunk_ordinal = Some(ordinal + 1);
                let previous_update = previous_updates.get(ordinal).copied().flatten();
                let original_contents = match previous_update {
                    // A repeated update edits its predecessor's result. When the
                    // predecessor failed, that failure is reported and no source
                    // remains to check this hunk against.
                    Some(previous) => match prepared.get(previous) {
                        Some(Some(AppliedPatch { new_contents, .. })) => {
                            String::clone(new_contents)
                        }
                        _ => {
                            prepared.push(None);
                            continue;
                        }
                    },
                    // A read failure applies to every chunk alike; report it once.
                    None => match read_update_source(&path, fs, sandbox).await {
                        Ok(contents) => contents,
                        Err(error) => {
                            failures.push(error);
                            prepared.push(None);
                            continue;
                        }
                    },
                };
                let source = PreparedSource::new(&original_contents);
                match new_contents_from_prepared(&path, &source, chunks, hunk_ordinal) {
                    Ok(new_contents) => {
                        let update = AppliedPatch {
                            original_contents,
                            new_contents,
                        };
                        // Validate the complete candidate before any hunk is
                        // published. Existing JSONC and malformed documents can
                        // still be edited; only strict-JSON regressions fail.
                        if let Some(error) = update_json_regression(
                            hunk.path(),
                            ordinal,
                            previous_updates,
                            &prepared,
                            &update,
                        ) {
                            failures.push(ApplyPatchError::ComputeReplacements(format!(
                                "{} was valid JSON before this patch; candidate is invalid: {error}. No files were changed.",
                                hunk.path().display()
                            )));
                            prepared.push(None);
                        } else {
                            prepared.push(Some(update));
                        }
                    }
                    Err(mut error) => {
                        mark_chained_mismatch(&mut error, previous_update);
                        // Keep the ordered matching error, then enumerate later
                        // independently broken chunks against this same snapshot.
                        // Chunks through the failing one either matched in order
                        // or are that failure, so alone they add nothing new.
                        let failed_chunks = match &error {
                            ApplyPatchError::PatchContextMismatch(mismatch) => {
                                mismatch.chunk_ordinal
                            }
                            _ => 0,
                        };
                        let first = error.to_string();
                        failures.push(error);
                        for (index, chunk) in chunks.iter().enumerate().skip(failed_chunks) {
                            if let Err(mut error) = new_contents_from_prepared(
                                &path,
                                &source,
                                std::slice::from_ref(chunk),
                                hunk_ordinal,
                            ) {
                                if let ApplyPatchError::PatchContextMismatch(ref mut mismatch) =
                                    error
                                {
                                    // Alone, a later chunk loses the anchor its
                                    // predecessors give it in sequence, so an
                                    // ambiguity found here is not independent.
                                    if index > 0
                                        && mismatch.kind == PatchContextMismatchKind::AmbiguousMatch
                                    {
                                        continue;
                                    }
                                    mismatch.chunk_ordinal = index + 1;
                                }
                                mark_chained_mismatch(&mut error, previous_update);
                                if error.to_string() != first
                                    && !failures
                                        .iter()
                                        .any(|prior| prior.to_string() == error.to_string())
                                {
                                    failures.push(error);
                                }
                            }
                        }
                        prepared.push(None);
                    }
                }
            }
            Hunk::DeleteFile { .. } => match fs.read_file_text(&path, sandbox).await {
                Ok(contents) => prepared.push(Some(AppliedPatch {
                    original_contents: contents,
                    new_contents: String::new(),
                })),
                Err(source) => {
                    failures.push(ApplyPatchError::IoError(IoError {
                        context: format!(
                            "Failed to delete file {}",
                            path.inferred_native_path_string()
                        ),
                        source,
                    }));
                    prepared.push(None);
                }
            },
            Hunk::AddFile { .. } => {
                match fs.get_metadata(&path, sandbox).await {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => failures.push(ApplyPatchError::ComputeReplacements(format!(
                        "Add File is create-only; {} already exists. Use Update File with a current codex-range source hash. No files were changed.",
                        path.inferred_native_path_string(),
                    ))),
                    Err(source) => failures.push(ApplyPatchError::IoError(IoError {
                        context: format!("Failed to inspect addition {}", path.inferred_native_path_string()),
                        source,
                    })),
                }
                prepared.push(None);
            }
        }
    }
    match failures.len() {
        0 => Ok(prepared),
        1 => Err(failures.remove(0)),
        _ => {
            let report = format!(
                "Patch preflight found {} conflicts; no files were changed.\n{}",
                failures.len(),
                failures
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n\n")
            );
            // Keep the typed mismatches in declaration order. Without them the
            // retry receipt reports no observed source, which costs a re-read
            // round trip that the single-conflict path does not need.
            let mismatches = failures
                .iter()
                .filter_map(|failure| match failure {
                    ApplyPatchError::PatchContextMismatch(mismatch) => Some(mismatch.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if mismatches.is_empty() {
                Err(ApplyPatchError::ComputeReplacements(report))
            } else {
                Err(ApplyPatchError::MultiplePatchConflicts { report, mismatches })
            }
        }
    }
}

/// Capture every publication endpoint before any write, including absence.
pub(crate) async fn capture_destination_preimages(
    hunks: &[Hunk],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> Result<HashMap<PathUri, Option<String>>, ApplyPatchError> {
    let mut destinations = HashMap::new();
    for hunk in hunks {
        let path = match hunk {
            Hunk::AddFile { .. } => hunk.resolve_path(cwd)?,
            Hunk::UpdateFile { move_path: Some(path), .. } => cwd.join(&path.to_string_lossy())?,
            _ => continue,
        };
        let content = match fs.read_file_text(&path, sandbox).await {
            Ok(content) => Some(content),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(ApplyPatchError::IoError(IoError {
                context: format!("Failed to read patch destination {path}"), source,
            })),
        };
        destinations.insert(path, content);
    }
    Ok(destinations)
}

/// The first of the repeated in-place updates of one file that hunk `index`
/// belongs to; `index` itself when no earlier hunk updates its file.
pub(crate) fn first_update(previous_updates: &[Option<usize>], mut index: usize) -> usize {
    while let Some(previous) = previous_updates.get(index).copied().flatten() {
        index = previous;
    }
    index
}

/// Labels a mismatch in a hunk that edits an earlier hunk's result, which
/// exists only in memory, so its lines and hash are not read as the file's.
fn mark_chained_mismatch(error: &mut ApplyPatchError, previous_update: Option<usize>) {
    if let (Some(previous), ApplyPatchError::PatchContextMismatch(mismatch)) =
        (previous_update, error)
    {
        mismatch.edits_result_of_hunk = Some(previous + 1);
        mismatch.message.push_str(&format!(
            "\nThis hunk edits the result of hunk {} for the same file; the current lines and hash describe that result.",
            previous + 1
        ));
    }
}

/// The strict-JSON regression an update introduces. Repeated updates of one
/// file are checked once, from the source the first read to the last result,
/// because an intermediate result may be incomplete.
fn update_json_regression(
    path: &std::path::Path,
    index: usize,
    previous_updates: &[Option<usize>],
    prepared: &[Option<AppliedPatch>],
    update: &AppliedPatch,
) -> Option<serde_json::Error> {
    if previous_updates.contains(&Some(index)) {
        return None;
    }
    let first = first_update(previous_updates, index);
    let source = if first == index {
        &update.original_contents
    } else {
        &prepared.get(first)?.as_ref()?.original_contents
    };
    invalid_json_candidate(path, source, &update.new_contents)
}

/// A verified update rebuilt against a newer source may land only where each
/// textual chunk still identifies one location. Within one snapshot, tier
/// precedence and the nearest match after an earlier chunk choose correctly;
/// across a concurrent edit they can move the approved change onto another
/// writer's copy of its lines. An unchanged result needs no check.
fn ensure_rebased_updates_keep_their_sites(
    hunks: &[Hunk],
    cwd: &PathUri,
    previous_updates: &[Option<usize>],
    prepared: &[Option<AppliedPatch>],
    action: &ApplyPatchAction,
) -> anyhow::Result<()> {
    // Repeated updates of one file were verified as one change under the first
    // hunk's path, ending with the last hunk's result.
    let mut last_updates = HashMap::new();
    for (index, hunk) in hunks.iter().enumerate() {
        if matches!(hunk, Hunk::UpdateFile { .. }) {
            last_updates.insert(first_update(previous_updates, index), index);
        }
    }
    for (index, hunk) in hunks.iter().enumerate() {
        let Hunk::UpdateFile { chunks, .. } = hunk else {
            continue;
        };
        let first = first_update(previous_updates, index);
        let path = hunks[first].resolve_path(cwd)?;
        let verified = match action.changes().get(&path) {
            Some(ApplyPatchFileChange::Update { new_content, .. }) => Some(new_content),
            _ => None,
        };
        let current = last_updates
            .get(&first)
            .and_then(|last| prepared.get(*last)?.as_ref())
            .map(|update| &update.new_contents);
        if verified.is_some() && verified == current {
            continue;
        }
        let source = prepared
            .get(index)
            .and_then(Option::as_ref)
            .context("update was not prepared before writes")?;
        if let Some(chunk) = chunk_without_unique_site(&source.original_contents, chunks) {
            anyhow::bail!(
                "{} changed after verification, and chunk {chunk} of hunk {} no longer matches exactly one location; no files were changed. Re-read the current source and regenerate the patch.",
                path.inferred_native_path_string(),
                index + 1
            );
        }
    }
    Ok(())
}

/// The 1-based ordinal of the first textual chunk whose site, its `@@` anchor
/// line followed by its old lines, `source` does not contain exactly once at
/// any match tier. Revision-bound ranges are checked by their whole-file hash.
fn chunk_without_unique_site(source: &str, chunks: &[UpdateFileChunk]) -> Option<usize> {
    let lines = PreparedSource::new(source).logical_lines;
    chunks
        .iter()
        .position(|chunk| {
            let context = chunk.change_context.as_ref();
            if context.is_some_and(|context| context.starts_with("codex-range ")) {
                return false;
            }
            let site = context
                .into_iter()
                .chain(&chunk.old_lines)
                .cloned()
                .collect::<Vec<_>>();
            !site.is_empty()
                && seek_sequence::candidate_starts_at_any_tier(&lines, &site, 2).len() != 1
        })
        .map(|index| index + 1)
}

async fn preimage_mode(path: &PathUri, fs: &dyn ExecutorFileSystem) -> Option<String> {
    if !fs.uses_host_native_paths() { return None; }
    let metadata = tokio::fs::symlink_metadata(path.to_abs_path().ok()?).await.ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() { return None; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 { return Some("100755".into()); }
    }
    Some("100644".into())
}

async fn ensure_not_directory(
    path: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> io::Result<FileMetadata> {
    let metadata = fs.get_metadata(path, sandbox).await?;
    if metadata.is_directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is a directory",
        ));
    }
    Ok(metadata)
}

async fn remove_failure_was_side_effect_free(
    path: &PathUri,
    expected_content: Option<&str>,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> bool {
    match expected_content {
        Some(expected_content) => fs
            .read_file_text(path, sandbox)
            .await
            .is_ok_and(|content| content == expected_content),
        None => fs
            .get_metadata(path, sandbox)
            .await
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
    }
}

async fn read_optional_file_text_for_delta(
    path: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    exact: &mut bool,
) -> io::Result<Option<String>> {
    match fs.read_file_text(path, sandbox).await {
        Ok(content) => {
            note_existing_path_delta_support(path, fs, sandbox, exact).await;
            Ok(Some(content))
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            // A dangling symlink is not an absent destination. Do not infer
            // absence merely because reading through it failed.
            match fs.get_metadata(path, sandbox).await {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
                Ok(_) => Err(source),
            }
        }
        Err(source) => Err(io::Error::new(
            source.kind(),
            format!(
                "refusing to overwrite {}: destination preimage is unreadable: {source}",
                path.inferred_native_path_string(),
            ),
        )),
    }
}

async fn note_existing_path_delta_support(
    path: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    exact: &mut bool,
) {
    match fs.get_metadata(path, sandbox).await {
        Ok(metadata) if metadata.is_file && !metadata.is_symlink => {}
        Ok(_) => *exact = false,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(_) => *exact = false,
    }
}

async fn write_file_with_missing_parent_retry(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    expected: Option<&str>,
    contents: Vec<u8>,
    sandbox: Option<&FileSystemSandboxContext>,
    exact: &mut bool,
) -> anyhow::Result<()> {
    match conditional_mutation(fs, path, expected, Some(contents.clone()), sandbox, exact).await {
        Ok(()) => Ok(()),
        Err(err)
            if err
                .downcast_ref::<io::Error>()
                .is_some_and(|err| err.kind() == io::ErrorKind::NotFound) =>
        {
            if let Some(parent) = path.parent() {
                fs.create_directory(&parent, CreateDirectoryOptions { recursive: true }, sandbox)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to create parent directories for {}",
                            path.inferred_native_path_string()
                        )
                    })?;
            }
            conditional_mutation(fs, path, expected, Some(contents), sandbox, exact)
                .await
                .with_context(|| {
                    format!(
                        "Failed to write file {}",
                        path.inferred_native_path_string()
                    )
                })?;
            Ok(())
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to write file {}",
                path.inferred_native_path_string()
            )
        }),
    }
}

#[derive(Debug, Error)]
#[error(
    "mutation requires recovery: {message}. Actual displaced file retained at {retained_path}; do not retry or restore over a competing writer"
)]
struct PatchRecoveryRequired {
    path: PathUri,
    retained_path: String,
    message: String,
}

#[derive(Debug, Error)]
#[error("source changed at mutation boundary; refusing to replace/remove {path}")]
struct PatchNoWriteConflict {
    path: PathUri,
}

async fn conditional_mutation(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    expected: Option<&str>,
    replacement: Option<Vec<u8>>,
    sandbox: Option<&FileSystemSandboxContext>,
    exact: &mut bool,
) -> anyhow::Result<()> {
    use codex_exec_server::FileMutationOutcome;
    match fs
        .mutate_file_if_matches(
            path,
            expected.map(|text| text.as_bytes().to_vec()),
            replacement,
            sandbox,
        )
        .await?
    {
        FileMutationOutcome::Committed => Ok(()),
        FileMutationOutcome::Conflict => Err(PatchNoWriteConflict { path: path.clone() }.into()),
        FileMutationOutcome::RecoveryRequired {
            retained_path,
            message,
        } => {
            *exact = false;
            Err(PatchRecoveryRequired {
                path: path.clone(),
                retained_path: retained_path.display().to_string(),
                message,
            }
            .into())
        }
    }
}

struct AppliedPatch {
    original_contents: String,
    new_contents: String,
}

fn invalid_json_candidate(
    path: &std::path::Path,
    original_contents: &str,
    new_contents: &str,
) -> Option<serde_json::Error> {
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        && serde_json::from_str::<serde::de::IgnoredAny>(original_contents).is_ok()
    {
        serde_json::from_str::<serde::de::IgnoredAny>(new_contents).err()
    } else {
        None
    }
}

/// Read the file at `path` and derive its contents after applying the chunks.
async fn derive_new_contents_from_chunks(
    path: &PathUri,
    chunks: &[UpdateFileChunk],
    hunk_ordinal: Option<usize>,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<AppliedPatch, ApplyPatchError> {
    let original_contents = read_update_source(path, fs, sandbox).await?;
    let new_contents = new_contents_from_chunks(path, &original_contents, chunks, hunk_ordinal)?;
    if let Some(error) =
        invalid_json_candidate(&path.to_path_buf(), &original_contents, &new_contents)
    {
        // Rebuilding against a newer snapshot must enforce the same guard as
        // preflight. Earlier hunks may already be committed at this point.
        return Err(ApplyPatchError::ComputeReplacements(format!(
            "{} was valid JSON before this patch; candidate is invalid: {error}",
            path.inferred_native_path_string(),
        )));
    }
    Ok(AppliedPatch {
        original_contents,
        new_contents,
    })
}

async fn read_update_source(
    path: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<String, ApplyPatchError> {
    fs.read_file_text(path, sandbox).await.map_err(|err| {
        ApplyPatchError::IoError(IoError {
            context: format!(
                "Failed to read file to update {}",
                path.inferred_native_path_string()
            ),
            source: err,
        })
    })
}

/// Return the contents that result from applying `chunks` to one snapshot of
/// the file at `path`.
fn new_contents_from_chunks(
    path: &PathUri,
    original_contents: &str,
    chunks: &[UpdateFileChunk],
    hunk_ordinal: Option<usize>,
) -> std::result::Result<String, ApplyPatchError> {
    new_contents_from_prepared(path, &PreparedSource::new(original_contents), chunks, hunk_ordinal)
}

struct PreparedSource<'a> {
    contents: &'a str,
    physical_lines: Vec<String>,
    logical_lines: Vec<String>,
    newline: &'static str,
    hash: std::cell::OnceCell<String>,
}

#[cfg(test)]
thread_local! {
    static SOURCE_PREPARATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl<'a> PreparedSource<'a> {
    fn new(contents: &'a str) -> Self {
        #[cfg(test)]
        SOURCE_PREPARATIONS.with(|count| count.set(count.get() + 1));
        let physical_lines = contents.split_inclusive('\n').map(String::from).collect::<Vec<_>>();
        let logical_lines = physical_lines.iter().map(|line| line.strip_suffix("\r\n")
            .or_else(|| line.strip_suffix('\n')).unwrap_or(line).to_string()).collect();
        let newline = if physical_lines.iter().find(|line| line.ends_with('\n'))
            .is_some_and(|line| line.ends_with("\r\n")) { "\r\n" } else { "\n" };
        Self { contents, physical_lines, logical_lines, newline, hash: std::cell::OnceCell::new() }
    }
}

fn new_contents_from_prepared(
    path: &PathUri,
    source: &PreparedSource<'_>,
    chunks: &[UpdateFileChunk],
    hunk_ordinal: Option<usize>,
) -> std::result::Result<String, ApplyPatchError> {
    let original_contents = source.contents;
    if chunks.is_empty() {
        return Ok(original_contents.to_string());
    }

    // Match logical lines, but keep the original physical lines for untouched
    // regions so updates do not normalize their line endings.
    let newline = source.newline;

    let path_text = path.inferred_native_path_string();
    let mut replacements = compute_replacements_with_hash(
        &source.logical_lines,
        original_contents,
        path,
        &path_text,
        hunk_ordinal,
        chunks,
        &source.hash,
    )?;
    for (_, _, lines) in &mut replacements {
        for line in lines {
            line.push_str(newline);
        }
    }
    let mut new_lines = apply_replacements(source.physical_lines.clone(), &replacements);
    let last_index = new_lines.len().saturating_sub(1);
    for (index, line) in new_lines.iter_mut().enumerate() {
        if index < last_index && !line.ends_with('\n') {
            // Appending after an unterminated last line needs a separator.
            line.push_str(newline);
        }
    }
    if !original_contents.is_empty()
        && !original_contents.ends_with('\n')
        && let Some(last) = new_lines.last_mut()
        && last.ends_with('\n')
    {
        let ending_len = if last.ends_with("\r\n") { 2 } else { 1 };
        last.truncate(last.len() - ending_len);
    }
    Ok(new_lines.concat())
}

/// Compute a list of replacements needed to transform `original_lines` into the
/// new lines, given the patch `chunks`. Each replacement is returned as
/// `(start_index, old_len, new_lines)`.
#[cfg(test)]
fn compute_replacements(
    original_lines: &[String],
    original_contents: &str,
    path_uri: &PathUri,
    path: &str,
    hunk_ordinal: Option<usize>,
    chunks: &[UpdateFileChunk],
) -> std::result::Result<Vec<(usize, usize, Vec<String>)>, ApplyPatchError> {
    compute_replacements_with_hash(original_lines, original_contents, path_uri, path, hunk_ordinal,
        chunks, &std::cell::OnceCell::new())
}

fn compute_replacements_with_hash(
    original_lines: &[String],
    original_contents: &str,
    path_uri: &PathUri,
    path: &str,
    hunk_ordinal: Option<usize>,
    chunks: &[UpdateFileChunk],
    source_hash: &std::cell::OnceCell<String>,
) -> std::result::Result<Vec<(usize, usize, Vec<String>)>, ApplyPatchError> {
    let matcher = ChunkMatcher {
        original_lines,
        original_contents,
        path_uri,
        path,
        hunk_ordinal,
        chunks,
        source_hash,
    };
    let mut cursor = ChunkCursor::default();
    matcher.match_from(0, &mut cursor, Pass::Primary)?;
    Ok(cursor.into_sorted_replacements())
}

/// Candidate sites tried when an unanchored search is ambiguous. Each costs one
/// pass over the hunk's remaining chunks against the same snapshot.
const DISAMBIGUATION_CANDIDATE_LIMIT: usize = 8;
/// Candidate line numbers listed in an ambiguity report.
const AMBIGUITY_REPORT_LIMIT: usize = 16;
/// Candidates rendered with a revision-bound anchor and source excerpt.
const AMBIGUITY_EXCERPT_LIMIT: usize = 4;

/// Source lines `start..end` (zero-based) that a matched chunk covers,
/// including its textual anchor, so a chunk listed out of file order cannot
/// be relocated onto another chunk's lines.
#[derive(Clone, Copy)]
struct ClaimedSpan {
    start: usize,
    end: usize,
    chunk_ordinal: usize,
}

#[derive(Clone)]
struct Replacement {
    start: usize,
    old_len: usize,
    new_lines: Vec<String>,
    /// The producing chunk's position in file order. Replacements that share
    /// a start apply in file order, even when a chunk was listed out of order.
    order: usize,
}

/// Matching progress through one hunk's chunks against one source snapshot.
#[derive(Clone, Default)]
struct ChunkCursor {
    replacements: Vec<Replacement>,
    claimed: Vec<ClaimedSpan>,
    line_index: usize,
    /// A search that follows an earlier match in this hunk is anchored: the
    /// nearest candidate is the intended one. Only a search that no match
    /// precedes must identify its location uniquely.
    anchored: bool,
}

impl ChunkCursor {
    fn push(&mut self, start: usize, old_len: usize, new_lines: Vec<String>, order: usize) {
        self.replacements.push(Replacement {
            start,
            old_len,
            new_lines,
            order,
        });
    }

    /// Stable: replacements with the same start and chunk keep their order.
    /// For chunks matched in file order this is the order they were listed.
    fn sorted(&self) -> Vec<&Replacement> {
        let mut sorted = self.replacements.iter().collect::<Vec<_>>();
        sorted.sort_by_key(|replacement| (replacement.start, replacement.order));
        sorted
    }

    fn into_sorted_replacements(self) -> Vec<(usize, usize, Vec<String>)> {
        let mut replacements = self.replacements;
        replacements.sort_by_key(|replacement| (replacement.start, replacement.order));
        replacements
            .into_iter()
            .map(|replacement| (replacement.start, replacement.old_len, replacement.new_lines))
            .collect()
    }
}

/// Where a textual chunk applies: its optional `@@` anchor line and the first
/// line of its old lines (or its insertion point).
#[derive(Clone, Copy)]
struct ChunkSite {
    anchor: Option<usize>,
    start: usize,
}

/// A search that matched more than one location at its strongest tier.
struct Ambiguity<'a> {
    error: seek_sequence::AmbiguousMatch,
    pattern: &'a [String],
    /// The textual `@@` anchor, rather than the old lines, was ambiguous.
    context: bool,
    start: usize,
    eof: bool,
    /// Set when the chunk was not found after the previous chunk's match and
    /// the whole-file search for an out-of-order site was ambiguous.
    out_of_order_after: Option<usize>,
}

enum Located<'a> {
    Site(ChunkSite),
    /// Listed out of file order: found only before the previous chunk's match.
    Relocated(ChunkSite),
    Ambiguous(Ambiguity<'a>),
}

/// Why a textual chunk has no site.
enum Missing {
    Context,
    /// The old lines do not immediately follow the anchor; one-based line.
    AfterAnchor(usize),
    Lines,
}

enum ChunkFlow {
    Next,
    /// Disambiguation already matched every remaining chunk of the hunk.
    Finished,
}

/// How far matching may depart from the listed chunk order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// The hunk itself: a chunk found only before the previous chunk's match
    /// may be relocated, and an ambiguous first chunk tries each candidate.
    Primary,
    /// Trying a candidate with the remaining chunks in listed order.
    InOrder,
    /// Trying a candidate when none fits the listed order.
    Relocating,
}

/// Matches one hunk's chunks against one immutable source snapshot.
struct ChunkMatcher<'a> {
    original_lines: &'a [String],
    original_contents: &'a str,
    path_uri: &'a PathUri,
    path: &'a str,
    hunk_ordinal: Option<usize>,
    chunks: &'a [UpdateFileChunk],
    // Keep hashing lazy for ordinary context patches, and still compare every
    // supplied range hash.
    source_hash: &'a std::cell::OnceCell<String>,
}

impl<'a> ChunkMatcher<'a> {
    fn source_hash(&self) -> &str {
        self.source_hash.get_or_init(|| {
            #[cfg(test)]
            tests::SOURCE_GUARD_HASH_CALLS.with(|calls| calls.set(calls.get() + 1));
            hex::encode(Sha256::digest(self.original_contents.as_bytes()))
        })
    }

    fn source(&self) -> PatchMismatchSource<'a> {
        PatchMismatchSource {
            original_lines: self.original_lines,
            original_contents: self.original_contents,
            path: self.path_uri,
            hunk_ordinal: self.hunk_ordinal,
            source_hash: Some(self.source_hash),
        }
    }

    fn match_from(
        &self,
        first: usize,
        cursor: &mut ChunkCursor,
        pass: Pass,
    ) -> std::result::Result<(), ApplyPatchError> {
        for chunk_index in first..self.chunks.len() {
            if let ChunkFlow::Finished = self.match_chunk(chunk_index, cursor, pass)? {
                break;
            }
        }
        Ok(())
    }

    fn match_chunk(
        &self,
        chunk_index: usize,
        cursor: &mut ChunkCursor,
        pass: Pass,
    ) -> std::result::Result<ChunkFlow, ApplyPatchError> {
        let chunk = &self.chunks[chunk_index];
        if let Some(handle) = chunk
            .change_context
            .as_deref()
            .and_then(|s| s.strip_prefix("codex-range "))
        {
            self.match_range(chunk_index, handle, cursor)?;
            return Ok(ChunkFlow::Next);
        }
        // A trial only learns whether the chunk fits, so it skips diagnostics.
        let trial_miss = || ApplyPatchError::ComputeReplacements(String::new());
        let located = match self.locate(chunk_index, cursor, pass != Pass::InOrder) {
            Ok(located) => located,
            Err(_) if pass != Pass::Primary => return Err(trial_miss()),
            Err(Missing::Context) => return Err(self.context_not_found(chunk_index)),
            Err(Missing::AfterAnchor(position)) => {
                return Err(self.not_following_anchor(chunk_index, position));
            }
            Err(Missing::Lines) => return Err(self.lines_not_found(chunk_index)),
        };
        match located {
            Located::Site(site) => self.place(chunk_index, site, cursor, /*relocated*/ false)?,
            Located::Relocated(site) => {
                self.place(chunk_index, site, cursor, /*relocated*/ true)?
            }
            // A trial never nests another candidate search.
            Located::Ambiguous(_) if pass != Pass::Primary => return Err(trial_miss()),
            Located::Ambiguous(ambiguity) => {
                // An out-of-order site must be unique on its own.
                if ambiguity.out_of_order_after.is_some() {
                    let candidates = self.candidate_starts(&ambiguity);
                    return Err(self.ambiguous_error(chunk_index, &ambiguity, &candidates, None));
                }
                return self.disambiguate(chunk_index, cursor, &ambiguity);
            }
        }
        Ok(ChunkFlow::Next)
    }

    fn match_range(
        &self,
        chunk_index: usize,
        handle: &str,
        cursor: &mut ChunkCursor,
    ) -> std::result::Result<(), ApplyPatchError> {
        let chunk = &self.chunks[chunk_index];
        let original_lines = self.original_lines;
        let invalid = |kind, message: &str, excerpt| {
            located_patch_mismatch(
                self.source(),
                chunk_index + 1,
                kind,
                message.into(),
                excerpt,
            )
        };
        let malformed = || {
            invalid(
                PatchContextMismatchKind::MalformedRange,
                "Malformed codex-range handle; expected START:END sha256:<64 hexadecimal characters>",
                None,
            )
        };
        let (range, hash) = handle.split_once(" sha256:").ok_or_else(malformed)?;
        let (start, end) = range.split_once(':').ok_or_else(malformed)?;
        let start = start.parse::<usize>().map_err(|_| malformed())?;
        let end = end.parse::<usize>().map_err(|_| malformed())?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(malformed());
        }
        if start == 0 || end < start || end > original_lines.len() {
            return Err(invalid(
                PatchContextMismatchKind::RangeOutOfBounds,
                "codex-range bounds must name an inclusive region within the current source",
                None,
            ));
        }
        let excerpt = || bounded_source_excerpt(original_lines, start - 1, end);
        if start - 1 < cursor.line_index {
            return Err(invalid(
                PatchContextMismatchKind::RangeOrderMismatch,
                "codex-range overlaps or precedes an earlier chunk; ranges must be ordered and nonoverlapping",
                excerpt(),
            ));
        }
        if hash != self.source_hash() {
            return Err(invalid(
                PatchContextMismatchKind::StaleRange,
                "codex-range source hash changed; reconcile current source before editing",
                excerpt(),
            ));
        }
        if !chunk.old_lines.is_empty() && chunk.old_lines != original_lines[start - 1..end] {
            return Err(invalid(
                PatchContextMismatchKind::RangeOldLinesMismatch,
                "codex-range old lines do not match the requested region; no relocation was attempted",
                excerpt(),
            ));
        }
        cursor.push(start - 1, end - start + 1, chunk.new_lines.clone(), start - 1);
        cursor.claimed.push(ClaimedSpan {
            start: start - 1,
            end,
            chunk_ordinal: chunk_index + 1,
        });
        cursor.line_index = end;
        cursor.anchored = true;
        Ok(())
    }

    /// Find where a textual chunk applies. An anchored search takes the nearest
    /// candidate after the previous chunk. When it finds nothing, the chunk may
    /// be listed out of file order: with `relocate`, it moves only to a site
    /// that is unique in the whole file, and `place` refuses a site another
    /// chunk claims.
    fn locate(
        &self,
        chunk_index: usize,
        cursor: &ChunkCursor,
        relocate: bool,
    ) -> std::result::Result<Located<'a>, Missing> {
        let chunks = self.chunks;
        let chunk = &chunks[chunk_index];
        let lines = self.original_lines;
        let mut relocated = false;
        let mut anchor = None;
        if let Some(context) = &chunks[chunk_index].change_context {
            let pattern = std::slice::from_ref(context);
            let ambiguous = |error, start, out_of_order_after| {
                Located::Ambiguous(Ambiguity {
                    error,
                    pattern,
                    context: true,
                    start,
                    eof: false,
                    out_of_order_after,
                })
            };
            match seek_sequence::seek_sequence(
                lines,
                pattern,
                cursor.line_index,
                /*eof*/ false,
                cursor.anchored,
            ) {
                Ok(Some(index)) => anchor = Some(index),
                Ok(None) if cursor.anchored && relocate => {
                    match seek_sequence::seek_sequence(lines, pattern, 0, false, false) {
                        Ok(Some(index)) => {
                            anchor = Some(index);
                            relocated = true;
                        }
                        Ok(None) => return Err(Missing::Context),
                        Err(error) => return Ok(ambiguous(error, 0, Some(cursor.line_index))),
                    }
                }
                Ok(None) => return Err(Missing::Context),
                Err(error) => return Ok(ambiguous(error, cursor.line_index, None)),
            }
        }
        let site = |site| {
            if relocated {
                Located::Relocated(site)
            } else {
                Located::Site(site)
            }
        };
        if chunk.old_lines.is_empty() {
            // An anchor inserts immediately after itself; otherwise append.
            let start = anchor.map_or(lines.len(), |index| index + 1);
            return Ok(site(ChunkSite { anchor, start }));
        }
        if let Some(anchor_index) = anchor {
            return match self.following_anchor(chunk_index, anchor_index) {
                Some(start) => Ok(site(ChunkSite {
                    anchor: Some(anchor_index),
                    start,
                })),
                None => Err(Missing::AfterAnchor(anchor_index + 1)),
            };
        }
        // Parsed blank lines are source preconditions, not newline sentinels.
        let pattern: &'a [String] = &chunks[chunk_index].old_lines;
        let eof = chunk.is_end_of_file;
        let at_eof = |start: &usize| !eof || start + pattern.len() == lines.len();
        let ambiguous = |error, start, out_of_order_after| {
            Located::Ambiguous(Ambiguity {
                error,
                pattern,
                context: false,
                start,
                eof,
                out_of_order_after,
            })
        };
        match seek_sequence::seek_sequence(lines, pattern, cursor.line_index, eof, cursor.anchored)
        {
            Ok(found) => {
                if let Some(start) = found.filter(at_eof) {
                    return Ok(Located::Site(ChunkSite {
                        anchor: None,
                        start,
                    }));
                }
            }
            Err(error) => return Ok(ambiguous(error, cursor.line_index, None)),
        }
        // Nothing matched at or after the previous chunk's end, so any match
        // starts before it: the chunk was listed out of file order.
        if cursor.anchored && relocate {
            match seek_sequence::seek_sequence(lines, pattern, 0, eof, false) {
                Ok(Some(start)) if at_eof(&start) => {
                    return Ok(Located::Relocated(ChunkSite {
                        anchor: None,
                        start,
                    }));
                }
                Ok(_) => {}
                Err(error) => return Ok(ambiguous(error, 0, Some(cursor.line_index))),
            }
        }
        Err(Missing::Lines)
    }

    /// A textual anchor binds the following contiguous preimage, not an
    /// unbounded search through later siblings. Include intervening lines as
    /// context, or use a revision-bound range for a distant edit.
    fn following_anchor(&self, chunk_index: usize, anchor_index: usize) -> Option<usize> {
        let chunk = &self.chunks[chunk_index];
        let lines = self.original_lines;
        let position = anchor_index + 1;
        let end = position
            .saturating_add(chunk.old_lines.len())
            .min(lines.len());
        // The window admits only `position`, so the search cannot be ambiguous.
        seek_sequence::seek_sequence(
            &lines[..end],
            &chunk.old_lines,
            position,
            chunk.is_end_of_file,
            /*anchored*/ true,
        )
        .ok()
        .flatten()
        .filter(|start| !chunk.is_end_of_file || start + chunk.old_lines.len() == lines.len())
    }

    /// Record a located chunk's replacements. A relocated chunk leaves the
    /// cursor where in-order matching reached, and must not share a line with
    /// another chunk's match.
    fn place(
        &self,
        chunk_index: usize,
        site: ChunkSite,
        cursor: &mut ChunkCursor,
        relocated: bool,
    ) -> std::result::Result<(), ApplyPatchError> {
        let chunk = &self.chunks[chunk_index];
        let lines = self.original_lines;
        let path = self.path;
        let pattern: &[String] = &chunk.old_lines;
        let claim = ClaimedSpan {
            start: site.anchor.unwrap_or(site.start),
            end: site.start + pattern.len(),
            chunk_ordinal: chunk_index + 1,
        };
        if relocated
            && let Some(other) = cursor
                .claimed
                .iter()
                .find(|other| claim.start < other.end && other.start < claim.end)
        {
            return Err(self.relocation_conflict(chunk_index, claim, *other, cursor.line_index));
        }
        // A bare addition appends at EOF and leaves in-order matching unchanged;
        // it sorts where in-order matching had reached when it was listed.
        let bare_addition = site.anchor.is_none() && pattern.is_empty();
        let order = if bare_addition {
            cursor.line_index
        } else {
            claim.start
        };
        if pattern.is_empty() {
            if chunk.is_end_of_file && site.start != lines.len() {
                return Err(patch_context_mismatch(
                    self.source(),
                    chunk_index + 1,
                    chunk,
                    PatchContextMismatchKind::ExpectedLinesNotFound,
                    format!("End-of-file insertion anchor is not at the end of {path}"),
                ));
            }
            cursor.push(site.start, 0, chunk.new_lines.clone(), order);
        } else {
            let start_idx = site.start;
            let new_slice: &[String] = &chunk.new_lines;
            for operation in
                similar::capture_diff_slices(similar::Algorithm::RawMyers, pattern, new_slice)
            {
                // Context identifies the location; preserve its original bytes even
                // when it matched only after whitespace or Unicode normalization.
                if matches!(operation, similar::DiffOp::Equal { .. }) {
                    continue;
                }
                let old_range = operation.old_range();
                for old_index in old_range.clone() {
                    let expected = &pattern[old_index];
                    let actual = &lines[start_idx + old_index];
                    if !expected.trim().is_empty()
                        && expected[..expected.len() - expected.trim_start().len()]
                            != actual[..actual.len() - actual.trim_start().len()]
                    {
                        let line = start_idx + old_index;
                        return Err(located_patch_mismatch(
                            self.source(),
                            chunk_index + 1,
                            PatchContextMismatchKind::IndentationMismatch,
                            format!(
                                "Indentation differs on removed line {} in {path}; use the exact indentation in the current source excerpt.",
                                line + 1
                            ),
                            bounded_source_excerpt(lines, line, line + 1),
                        ));
                    }
                    if expected != actual {
                        let line = start_idx + old_index;
                        return Err(located_patch_mismatch(
                            self.source(),
                            chunk_index + 1,
                            PatchContextMismatchKind::RemovedLineMismatch,
                            format!(
                                "Removed line {} differs from current source in {path}; normalization is only allowed for unchanged context. Reconcile the exact current bytes before retrying.",
                                line + 1
                            ),
                            bounded_source_excerpt(lines, line, line + 1),
                        ));
                    }
                }
                cursor.push(
                    start_idx + old_range.start,
                    old_range.len(),
                    new_slice[operation.new_range()].to_vec(),
                    order,
                );
            }
        }
        if !bare_addition {
            cursor.claimed.push(claim);
            if !relocated {
                cursor.line_index = claim.end;
                cursor.anchored = true;
            }
        }
        Ok(())
    }

    /// An unanchored search matched several sites. Each candidate is tried with
    /// the rest of the hunk against the same snapshot, in listed order first;
    /// only when no candidate fits that order may later chunks be relocated.
    /// When exactly one candidate lets every remaining chunk apply, or all that
    /// do write the same bytes, the listed chunks select it; otherwise report
    /// every candidate.
    fn disambiguate(
        &self,
        chunk_index: usize,
        cursor: &mut ChunkCursor,
        ambiguity: &Ambiguity<'a>,
    ) -> std::result::Result<ChunkFlow, ApplyPatchError> {
        let candidates = self.candidate_starts(ambiguity);
        if candidates.len() > DISAMBIGUATION_CANDIDATE_LIMIT {
            return Err(self.ambiguous_error(chunk_index, ambiguity, &candidates, None));
        }
        let chunk = &self.chunks[chunk_index];
        let mut placed = Vec::new();
        // Removed lines must match exact bytes, but that exactness never
        // chooses a location: a candidate that rejects this chunk's own edit
        // leaves the ambiguity unresolved.
        let mut placed_everywhere = true;
        for &candidate in &candidates {
            let site = if !ambiguity.context {
                Some(ChunkSite {
                    anchor: None,
                    start: candidate,
                })
            } else if chunk.old_lines.is_empty() {
                Some(ChunkSite {
                    anchor: Some(candidate),
                    start: candidate + 1,
                })
            } else {
                // An anchor whose old lines do not follow it is not a site.
                self.following_anchor(chunk_index, candidate)
                    .map(|start| ChunkSite {
                        anchor: Some(candidate),
                        start,
                    })
            };
            let Some(site) = site else {
                continue;
            };
            let mut trial = cursor.clone();
            if self
                .place(chunk_index, site, &mut trial, /*relocated*/ false)
                .is_ok()
            {
                placed.push((candidate, trial));
            } else {
                placed_everywhere = false;
            }
        }
        let mut consistent = Vec::new();
        for pass in [Pass::InOrder, Pass::Relocating] {
            let mut viable = placed
                .iter()
                .filter_map(|(candidate, placed)| {
                    let mut trial = placed.clone();
                    self.match_from(chunk_index + 1, &mut trial, pass)
                        .is_ok()
                        .then_some((*candidate, trial))
                })
                .collect::<Vec<_>>();
            if viable.is_empty() {
                continue;
            }
            if placed_everywhere && self.write_same_bytes(&viable) {
                let (_, trial) = viable.swap_remove(0);
                *cursor = trial;
                return Ok(ChunkFlow::Finished);
            }
            consistent = viable.into_iter().map(|(candidate, _)| candidate).collect();
            break;
        }
        Err(self.ambiguous_error(chunk_index, ambiguity, &candidates, Some(&consistent)))
    }

    /// Whether every trial writes the same bytes.
    fn write_same_bytes(&self, trials: &[(usize, ChunkCursor)]) -> bool {
        let Some(((_, first), rest)) = trials.split_first() else {
            return false;
        };
        let output = self.trial_output(first);
        output.is_some()
            && rest
                .iter()
                .all(|(_, trial)| self.trial_output(trial) == output)
    }

    /// The lines a trial writes, where a kept source line (with its own line
    /// ending) never equals a patched line, so equal outputs are equal bytes.
    fn trial_output<'t>(&'t self, trial: &'t ChunkCursor) -> Option<Vec<(bool, &'t str)>> {
        let mut output = self
            .original_contents
            .split_inclusive('\n')
            .map(|line| (false, line))
            .collect::<Vec<_>>();
        if output.len() != self.original_lines.len() {
            return None;
        }
        for replacement in trial.sorted().into_iter().rev() {
            output.splice(
                replacement.start..replacement.start + replacement.old_len,
                replacement
                    .new_lines
                    .iter()
                    .map(|line| (true, line.as_str())),
            );
        }
        Some(output)
    }

    /// Zero-based candidate starts for an ambiguous search, one past the report
    /// limit so the report can say that more exist.
    fn candidate_starts(&self, ambiguity: &Ambiguity<'_>) -> Vec<usize> {
        let mut candidates = seek_sequence::ambiguous_candidates(
            self.original_lines,
            ambiguity.pattern,
            ambiguity.start,
            ambiguity.eof,
            &ambiguity.error,
            AMBIGUITY_REPORT_LIMIT + 1,
        );
        if candidates.len() < 2 {
            // Never report fewer candidates than the search found.
            candidates = vec![
                ambiguity.error.first_line - 1,
                ambiguity.error.second_line - 1,
            ];
        }
        candidates
    }

    fn ambiguous_error(
        &self,
        chunk_index: usize,
        ambiguity: &Ambiguity<'_>,
        candidates: &[usize],
        consistent: Option<&[usize]>,
    ) -> ApplyPatchError {
        let path = self.path;
        let lines = self.original_lines;
        let strictness = ambiguity.error.strictness;
        let truncated = candidates.len() > AMBIGUITY_REPORT_LIMIT;
        let listed = &candidates[..candidates.len().min(AMBIGUITY_REPORT_LIMIT)];
        let line_list = line_number_list(listed, truncated);
        let mut message = match ambiguity.out_of_order_after {
            None => format!(
                "Ambiguous {strictness} match at lines {line_list}; add more context to select one location in {path}."
            ),
            Some(after) => {
                let (subject, verb) = if ambiguity.context {
                    ("the textual anchor", "it matches")
                } else {
                    ("expected lines", "they match")
                };
                format!(
                    "Failed to find {subject} in {path} after line {after}, where the previous chunk matched; earlier in the file {verb} at lines {line_list} ({strictness} match). A chunk listed out of file order must match one location; add more context to select one location."
                )
            }
        };
        match consistent {
            Some([]) => message.push_str(
                " No candidate lets this chunk and the rest of this hunk apply; check each excerpt.",
            ),
            Some(consistent) if consistent.len() < candidates.len() => {
                let verb = if consistent.len() == 1 { "lets" } else { "let" };
                message.push_str(&format!(
                    " Only {} {} {verb} this chunk and the rest of this hunk apply.",
                    if consistent.len() == 1 { "line" } else { "lines" },
                    line_number_list(consistent, /*truncated*/ false),
                ));
            }
            _ => {}
        }
        message.push_str(
            " Choose a candidate and replace its entire range (including unchanged context) using one of these revision-bound anchors:",
        );
        // Render the candidates consistent with the rest of the hunk first.
        let mut rendered = consistent
            .unwrap_or_default()
            .iter()
            .chain(listed)
            .copied()
            .collect::<Vec<_>>();
        let mut seen = std::collections::BTreeSet::new();
        rendered.retain(|candidate| seen.insert(*candidate));
        let hash = self.source_hash();
        let pattern_len = ambiguity.pattern.len();
        for &start in rendered.iter().take(AMBIGUITY_EXCERPT_LIMIT) {
            let end = start + pattern_len;
            message.push_str(&format!(
                "\n@@ codex-range {}:{end} sha256:{hash}\n",
                start + 1
            ));
            if let Some((_, _, excerpt)) = bounded_source_excerpt(lines, start, end) {
                message.push_str(&excerpt);
            }
        }
        if rendered.len() > AMBIGUITY_EXCERPT_LIMIT {
            message.push_str(&format!(
                "\n[{} more candidates are listed above without excerpts]",
                rendered.len() - AMBIGUITY_EXCERPT_LIMIT
            ));
        }
        let first = rendered.first().copied().unwrap_or_default();
        located_patch_mismatch(
            self.source(),
            chunk_index + 1,
            PatchContextMismatchKind::AmbiguousMatch,
            message,
            bounded_source_excerpt(lines, first, first + 1),
        )
    }

    fn context_not_found(&self, chunk_index: usize) -> ApplyPatchError {
        let chunk = &self.chunks[chunk_index];
        let context = bounded_expected_lines(chunk.change_context.iter().map(String::as_str));
        let path = self.path;
        let region =
            bounded_patch_mismatch_region(self.original_lines, chunk, chunk.change_context.as_slice());
        let note = region
            .as_ref()
            .map(|(_, closeness)| closeness.note())
            .unwrap_or_default();
        located_patch_mismatch(
            self.source(),
            chunk_index + 1,
            PatchContextMismatchKind::ContextNotFound,
            format!("Failed to find context '{context}' in {path}{note}"),
            region.map(|(excerpt, _)| excerpt),
        )
    }

    fn not_following_anchor(&self, chunk_index: usize, position: usize) -> ApplyPatchError {
        let chunk = &self.chunks[chunk_index];
        let lines = self.original_lines;
        let path = self.path;
        let expected = bounded_expected_lines(chunk.old_lines.iter().map(String::as_str));
        located_patch_mismatch(
            self.source(),
            chunk_index + 1,
            PatchContextMismatchKind::ExpectedLinesNotFound,
            format!(
                "Expected lines must immediately follow the textual anchor in {path} at line {position}; no later sibling was searched. Include intervening source as context or use a current codex-range:\n{expected}"
            ),
            bounded_source_excerpt(
                lines,
                position.saturating_sub(1),
                position
                    .saturating_add(chunk.old_lines.len())
                    .min(lines.len()),
            ),
        )
    }

    /// The expected lines match nowhere in the file at any tier.
    fn lines_not_found(&self, chunk_index: usize) -> ApplyPatchError {
        let chunk = &self.chunks[chunk_index];
        let path = self.path;
        let expected = bounded_expected_lines(chunk.old_lines.iter().map(String::as_str));
        let region = bounded_patch_mismatch_region(self.original_lines, chunk, &chunk.old_lines);
        let note = region
            .as_ref()
            .map(|(_, closeness)| closeness.note())
            .unwrap_or_default();
        located_patch_mismatch(
            self.source(),
            chunk_index + 1,
            PatchContextMismatchKind::ExpectedLinesNotFound,
            format!("Failed to find expected lines in {path}{note}:\n{expected}"),
            region.map(|(excerpt, _)| excerpt),
        )
    }

    fn relocation_conflict(
        &self,
        chunk_index: usize,
        claim: ClaimedSpan,
        other: ClaimedSpan,
        line_index: usize,
    ) -> ApplyPatchError {
        let chunk = &self.chunks[chunk_index];
        let path = self.path;
        let expected = if chunk.old_lines.is_empty() {
            bounded_expected_lines(chunk.change_context.iter().map(String::as_str))
        } else {
            bounded_expected_lines(chunk.old_lines.iter().map(String::as_str))
        };
        located_patch_mismatch(
            self.source(),
            chunk_index + 1,
            PatchContextMismatchKind::ExpectedLinesNotFound,
            format!(
                "Failed to find expected lines in {path} after line {line_index}, where the previous chunk matched; their only earlier match, lines {}-{}, overlaps chunk {} at lines {}-{}. Merge the two chunks into one chunk listed in file order:\n{expected}",
                claim.start + 1,
                claim.end,
                other.chunk_ordinal,
                other.start + 1,
                other.end,
            ),
            bounded_source_excerpt(self.original_lines, claim.start, claim.end),
        )
    }
}

/// One-based line numbers for zero-based starts: "4", "4 and 7", "4, 7 and 9".
fn line_number_list(starts: &[usize], truncated: bool) -> String {
    let numbers = starts
        .iter()
        .map(|start| (start + 1).to_string())
        .collect::<Vec<_>>();
    match (numbers.as_slice(), truncated) {
        ([], _) => String::new(),
        (numbers, true) => format!("{} and more", numbers.join(", ")),
        ([only], false) => only.clone(),
        ([init @ .., last], false) => format!("{} and {last}", init.join(", ")),
    }
}

const PATCH_MISMATCH_CONTEXT_LINES: usize = 3;
const PATCH_MISMATCH_MAX_LINES: usize = 20;
const PATCH_MISMATCH_MAX_BYTES: usize = 4 * 1024;

fn bounded_expected_lines<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    const TRUNCATED: &str = "\n[expected lines truncated; reread the full target region]";
    let mut rendered = String::new();
    for (index, line) in lines.enumerate() {
        let separator_bytes = usize::from(index > 0);
        let remaining = PATCH_MISMATCH_MAX_BYTES
            .saturating_sub(rendered.len() + separator_bytes + TRUNCATED.len());
        if index >= PATCH_MISMATCH_MAX_LINES || remaining == 0 {
            rendered.push_str(TRUNCATED);
            break;
        }
        if index > 0 {
            rendered.push('\n');
        }
        rendered.push_str(&line[..line.floor_char_boundary(remaining)]);
        if line.len() > remaining {
            rendered.push_str(TRUNCATED);
            break;
        }
    }
    rendered
}

struct PatchMismatchSource<'a> {
    original_lines: &'a [String],
    original_contents: &'a str,
    path: &'a PathUri,
    hunk_ordinal: Option<usize>,
    source_hash: Option<&'a std::cell::OnceCell<String>>,
}

fn patch_context_mismatch(
    source: PatchMismatchSource<'_>,
    chunk_ordinal: usize,
    chunk: &UpdateFileChunk,
    kind: PatchContextMismatchKind,
    message: String,
) -> ApplyPatchError {
    let excerpt = bounded_patch_mismatch_excerpt(source.original_lines, chunk);
    located_patch_mismatch(source, chunk_ordinal, kind, message, excerpt)
}

fn located_patch_mismatch(
    source: PatchMismatchSource<'_>,
    chunk_ordinal: usize,
    kind: PatchContextMismatchKind,
    message: String,
    excerpt: Option<(usize, usize, String)>,
) -> ApplyPatchError {
    let Some(hunk_ordinal) = source.hunk_ordinal else {
        return ApplyPatchError::ComputeReplacements(message);
    };
    let (current_line_start, current_line_end, current_excerpt) = excerpt.unwrap_or_default();
    let hash = || {
        #[cfg(test)]
        tests::SOURCE_GUARD_HASH_CALLS.with(|calls| calls.set(calls.get() + 1));
        hex::encode(Sha256::digest(source.original_contents.as_bytes()))
    };
    let current_content_sha256 = source.source_hash.map_or_else(hash, |cache| cache.get_or_init(hash).clone());
    ApplyPatchError::PatchContextMismatch(PatchContextMismatch {
        kind,
        hunk_ordinal,
        chunk_ordinal,
        canonical_path: source.path.to_string(),
        current_content_sha256,
        current_line_start,
        current_line_end,
        current_excerpt,
        message,
        edits_result_of_hunk: None,
    })
}

fn bounded_patch_mismatch_excerpt(
    original_lines: &[String],
    chunk: &UpdateFileChunk,
) -> Option<(usize, usize, String)> {
    if let Some(excerpt) = bounded_exact_patch_mismatch_excerpt(original_lines, chunk) {
        return Some(excerpt);
    }
    // Diagnostics use the patcher's matching tiers, but never guess between
    // duplicate anchors. Bound fallback work independently of the exact pass.
    let probes = chunk.old_lines.len().saturating_add(1);
    if patch_mismatch_budget_exceeded(original_lines, probes) {
        return None;
    }
    for line in chunk
        .old_lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .chain(chunk.change_context.iter())
    {
        if let Ok(Some(index)) = seek_sequence::seek_sequence(
            original_lines,
            std::slice::from_ref(line),
            0,
            false,
            false,
        ) {
            return bounded_source_excerpt(original_lines, index, index + 1);
        }
    }
    None
}

/// Whether `passes` scans of the source exceed the diagnostic work budget.
fn patch_mismatch_budget_exceeded(original_lines: &[String], passes: usize) -> bool {
    original_lines.len().saturating_mul(passes) > 64 * 1024
        || original_lines
            .iter()
            .map(String::len)
            .sum::<usize>()
            .saturating_mul(passes)
            > 4 * 1024 * 1024
}

/// How a chunk's diagnostic region relates to the lines it expected.
#[derive(Debug, PartialEq, Eq)]
enum RegionCloseness {
    /// Where the expected lines first diverge from their best partial match,
    /// or a line that matches one of them uniquely.
    Located,
    /// The chunk's replacement lines already match current lines `start..end`.
    AlreadyApplied { start: usize, end: usize },
    /// Current lines `start..end` hold the expected code formatted differently.
    Rewrapped { start: usize, end: usize },
    /// The line most similar to an expected line, ahead of every other line.
    Similar { line: usize },
}

impl RegionCloseness {
    /// A parenthetical naming the region, for the failure message.
    fn note(&self) -> String {
        match *self {
            Self::Located => String::new(),
            Self::AlreadyApplied { start, end } if end - start == 1 => format!(
                " (the chunk's replacement lines already match current line {}; the change may already be applied)",
                start + 1
            ),
            Self::AlreadyApplied { start, end } => format!(
                " (the chunk's replacement lines already match current lines {}-{end}; the change may already be applied)",
                start + 1
            ),
            Self::Rewrapped { start, end } if end - start == 1 => format!(
                " (current line {} holds the same code formatted differently; copy it exactly)",
                start + 1
            ),
            Self::Rewrapped { start, end } => format!(
                " (current lines {}-{end} hold the same code formatted differently; copy them exactly)",
                start + 1
            ),
            Self::Similar { line } => format!(
                " (line {} is the closest current line and is shown below)",
                line + 1
            ),
        }
    }
}

/// The current region closest to `expected` lines that match nowhere, checked
/// in order: the chunk's replacement lines already present, the same code
/// formatted differently, the established mismatch excerpt, then the most
/// similar line. Each tier is bounded and never chooses between equally close
/// regions.
fn bounded_patch_mismatch_region(
    original_lines: &[String],
    chunk: &UpdateFileChunk,
    expected: &[String],
) -> Option<((usize, usize, String), RegionCloseness)> {
    let region = |start: usize, end: usize, closeness| {
        bounded_source_excerpt(original_lines, start, end).map(|excerpt| (excerpt, closeness))
    };
    if let Some(start) = already_applied_start(original_lines, chunk) {
        let end = start + chunk.new_lines.len();
        return region(start, end, RegionCloseness::AlreadyApplied { start, end });
    }
    if let Some((start, end)) = rewrapped_region(original_lines, expected) {
        return region(start, end, RegionCloseness::Rewrapped { start, end });
    }
    if let Some(excerpt) = bounded_patch_mismatch_excerpt(original_lines, chunk) {
        return Some((excerpt, RegionCloseness::Located));
    }
    let line = most_similar_line(original_lines, expected)?;
    region(line, line + 1, RegionCloseness::Similar { line })
}

/// Where the chunk's replacement lines match uniquely, when they contain text
/// that its expected lines lack.
fn already_applied_start(original_lines: &[String], chunk: &UpdateFileChunk) -> Option<usize> {
    let old_lines = chunk
        .old_lines
        .iter()
        .collect::<std::collections::HashSet<_>>();
    let adds_text = chunk
        .new_lines
        .iter()
        .any(|line| !line.trim().is_empty() && !old_lines.contains(line));
    if !adds_text || patch_mismatch_budget_exceeded(original_lines, /*passes*/ 1) {
        return None;
    }
    seek_sequence::seek_sequence(
        original_lines,
        &chunk.new_lines,
        0,
        chunk.is_end_of_file,
        /*anchored*/ false,
    )
    .ok()
    .flatten()
}

/// The one current region whose code equals the code of `expected` apart from
/// formatting (whitespace, line breaks, and a trailing comma before a closing
/// bracket), unless its lines already equal `expected` up to surrounding
/// whitespace.
fn rewrapped_region(original_lines: &[String], expected: &[String]) -> Option<(usize, usize)> {
    if patch_mismatch_budget_exceeded(original_lines, /*passes*/ 1) {
        return None;
    }
    let (text, starts) = formatting_free_text(original_lines);
    let (target, _) = formatting_free_text(expected);
    if target.is_empty() {
        return None;
    }
    // Overlapping occurrences count too; only those on line boundaries are
    // regions. Bound the search where the target occurs inside many lines.
    let mut searches = 1024usize;
    let mut found = None;
    let mut from = 0;
    while let Some(relative) = text.get(from..).and_then(|rest| rest.find(target.as_str())) {
        searches = searches.checked_sub(1)?;
        let offset = from + relative;
        from = offset + text[offset..].chars().next().map_or(1, char::len_utf8);
        let first = starts.partition_point(|&start| start < offset);
        let end = starts.partition_point(|&start| start < offset + target.len());
        if starts.get(first) != Some(&offset) || starts.get(end) != Some(&(offset + target.len())) {
            continue;
        }
        // Lines without code share the next line's start; begin at the line
        // that holds code.
        let mut start = first;
        while starts.get(start + 1) == Some(&offset) {
            start += 1;
        }
        if found.replace((start, end)).is_some() {
            return None;
        }
    }
    let (start, end) = found?;
    // Equal lines up to surrounding whitespace already match a seek tier, so
    // formatting is not why the chunk failed.
    let same_lines = end - start == expected.len()
        && original_lines[start..end]
            .iter()
            .zip(expected)
            .all(|(current, expected)| current.trim() == expected.trim());
    (!same_lines).then_some((start, end))
}

/// `lines` without whitespace or a comma before a closing bracket, as one
/// string, with the offset in it where each line and the end begin.
fn formatting_free_text(lines: &[String]) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut starts = Vec::with_capacity(lines.len() + 1);
    for line in lines {
        let mut start = None;
        for character in line.chars().filter(|character| !character.is_whitespace()) {
            // Formatters add a trailing comma when they wrap a list.
            if matches!(character, ')' | ']' | '}') && text.ends_with(',') {
                text.pop();
            }
            start.get_or_insert(text.len());
            text.push(character);
        }
        starts.push(start.unwrap_or(text.len()));
    }
    starts.push(text.len());
    // A comma removed for a later line moves the boundary before that line.
    for index in (0..lines.len()).rev() {
        starts[index] = starts[index].min(starts[index + 1]);
    }
    (text, starts)
}

/// The line most similar to an expected line by character-bigram Dice
/// coefficient, when that is at least 0.7 and ahead of every other line by at
/// least 0.05.
fn most_similar_line(original_lines: &[String], expected: &[String]) -> Option<usize> {
    // Shorter lines, such as braces, carry too little text to compare; the
    // longest expected lines are the most distinctive.
    const MIN_CHARS: usize = 8;
    const MAX_PROBES: usize = 8;
    let bigrams = |line: &str| {
        let characters = line.trim().chars().collect::<Vec<_>>();
        let mut pairs = characters
            .windows(2)
            .map(|pair| (pair[0], pair[1]))
            .collect::<Vec<_>>();
        pairs.sort_unstable();
        pairs
    };
    let mut probes = expected
        .iter()
        .map(|line| line.trim())
        .filter(|line| line.chars().count() >= MIN_CHARS)
        .collect::<Vec<_>>();
    probes.sort_unstable_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));
    probes.dedup();
    probes.truncate(MAX_PROBES);
    if probes.is_empty() || patch_mismatch_budget_exceeded(original_lines, probes.len()) {
        return None;
    }
    let probes = probes.into_iter().map(bigrams).collect::<Vec<_>>();
    let mut best: Option<(f64, usize)> = None;
    let mut runner_up = 0.0f64;
    for (index, line) in original_lines.iter().enumerate() {
        if line.trim().chars().count() < MIN_CHARS {
            continue;
        }
        let current = bigrams(line.as_str());
        let score = probes
            .iter()
            .map(|probe| bigram_dice(probe, &current))
            .fold(0.0, f64::max);
        match best {
            Some((top, _)) if score <= top => runner_up = runner_up.max(score),
            _ => {
                if let Some((top, _)) = best {
                    runner_up = runner_up.max(top);
                }
                best = Some((score, index));
            }
        }
    }
    let (score, line) = best?;
    (score >= 0.7 && score - runner_up >= 0.05).then_some(line)
}

/// Dice coefficient of two sorted bigram multisets.
fn bigram_dice(left: &[(char, char)], right: &[(char, char)]) -> f64 {
    let (mut left_index, mut right_index, mut common) = (0, 0, 0usize);
    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                left_index += 1;
                right_index += 1;
            }
        }
    }
    let total = left.len() + right.len();
    if total == 0 {
        return 0.0;
    }
    (2 * common) as f64 / total as f64
}

fn bounded_exact_patch_mismatch_excerpt(
    original_lines: &[String],
    chunk: &UpdateFileChunk,
) -> Option<(usize, usize, String)> {
    if original_lines.is_empty() {
        return None;
    }
    // Old-line offsets describe contiguous source, including blank lines.
    // Located textual-anchor failures supply their own bounded excerpt rather
    // than inferring a diagnostic location from a later sibling's old lines.
    let expected = if chunk.old_lines.is_empty() {
        std::slice::from_ref(chunk.change_context.as_ref()?)
    } else {
        chunk.old_lines.as_slice()
    };
    // A candidate start scores one per expected line equal to the current line
    // it aligns with. One pass over the source finds every equal pair, so a
    // long chunk does not cost a pass per expected line; blank expected lines
    // score but do not propose candidates.
    if original_lines.iter().map(String::len).sum::<usize>() > 16 * 1024 * 1024 {
        return None;
    }
    let mut expected_offsets = HashMap::<&str, Vec<usize>>::new();
    for (offset, line) in expected.iter().enumerate() {
        if !line.is_empty() {
            expected_offsets
                .entry(line.as_str())
                .or_default()
                .push(offset);
        }
    }
    let mut votes = 256 * 1024usize;
    let mut candidate_scores = BTreeMap::<usize, usize>::new();
    for (current_index, current_line) in original_lines.iter().enumerate() {
        let Some(offsets) = expected_offsets.get(current_line.as_str()) else {
            continue;
        };
        for &offset in offsets {
            if let Some(start) = current_index.checked_sub(offset) {
                votes = votes.checked_sub(1)?;
                *candidate_scores.entry(start).or_default() += 1;
            }
        }
    }
    let blank_offsets = expected
        .iter()
        .enumerate()
        .filter_map(|(offset, line)| line.is_empty().then_some(offset))
        .collect::<Vec<_>>();
    if !blank_offsets.is_empty() {
        for (start, score) in &mut candidate_scores {
            votes = votes.checked_sub(blank_offsets.len())?;
            *score += blank_offsets
                .iter()
                .filter(|offset| {
                    original_lines
                        .get(*start + **offset)
                        .is_some_and(String::is_empty)
                })
                .count();
        }
    }
    let best_score = candidate_scores.values().copied().max()?;
    let best = candidate_scores
        .into_iter()
        .filter_map(|(start, score)| (score == best_score).then_some(start))
        .collect::<Vec<_>>();
    let candidate_start = match best.as_slice() {
        [only] => *only,
        // Context leads with the line its author located, so when an insertion
        // displaces the rest of a chunk, only the true site keeps that anchor.
        // Identical candidates stay ambiguous.
        tied => {
            let (anchor_offset, anchor) = expected
                .iter()
                .enumerate()
                .find(|(_, line)| !line.is_empty())?;
            let mut anchored = tied
                .iter()
                .copied()
                .filter(|start| original_lines.get(start + anchor_offset) == Some(anchor));
            let start = anchored.next()?;
            if anchored.next().is_some() {
                return None;
            }
            start
        }
    };
    let mismatch = expected
        .iter()
        .enumerate()
        .find_map(|(offset, line)| {
            (original_lines.get(candidate_start + offset) != Some(line))
                .then_some(candidate_start + offset)
        })
        .unwrap_or(candidate_start)
        .min(original_lines.len() - 1);
    bounded_source_excerpt(original_lines, mismatch, mismatch + 1)
}

fn bounded_source_excerpt(
    original_lines: &[String],
    candidate_start: usize,
    expected_end: usize,
) -> Option<(usize, usize, String)> {
    if original_lines.is_empty() {
        return None;
    }
    let mut excerpt_start = candidate_start.saturating_sub(PATCH_MISMATCH_CONTEXT_LINES);
    let mut excerpt_end = expected_end
        .saturating_add(PATCH_MISMATCH_CONTEXT_LINES)
        .min(original_lines.len());
    if excerpt_end.saturating_sub(excerpt_start) > PATCH_MISMATCH_MAX_LINES {
        excerpt_end = excerpt_start + PATCH_MISMATCH_MAX_LINES;
    }
    if excerpt_start == excerpt_end {
        excerpt_start = candidate_start.min(original_lines.len() - 1);
        excerpt_end = excerpt_start + 1;
    }

    let mut rendered = String::new();
    let mut rendered_end = excerpt_start;
    for (index, line) in original_lines[excerpt_start..excerpt_end]
        .iter()
        .enumerate()
    {
        let line_number = excerpt_start + index + 1;
        let prefix = format!("{line_number:>6} | ");
        let separator_bytes = usize::from(!rendered.is_empty());
        let remaining = PATCH_MISMATCH_MAX_BYTES.saturating_sub(
            rendered
                .len()
                .saturating_add(prefix.len())
                .saturating_add(separator_bytes),
        );
        if remaining == 0 {
            break;
        }
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str(&prefix);
        rendered.push_str(&line[..line.floor_char_boundary(remaining)]);
        rendered_end = excerpt_start + index + 1;
        if line.len() > remaining {
            break;
        }
    }
    (!rendered.is_empty()).then(|| (excerpt_start + 1, rendered_end, rendered))
}

/// Apply the `(start_index, old_len, new_lines)` replacements to `original_lines`,
/// returning the modified file contents as a vector of lines.
fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    // We must apply replacements in descending order so that earlier replacements
    // don't shift the positions of later ones.
    for (start_idx, old_len, new_segment) in replacements.iter().rev() {
        let start_idx = *start_idx;
        let old_len = *old_len;

        lines.splice(start_idx..start_idx + old_len, new_segment.iter().cloned());
    }

    lines
}

/// Intended result of a file update for apply_patch.
#[derive(Debug, Eq, PartialEq)]
pub struct ApplyPatchFileUpdate {
    unified_diff: String,
    original_content: String,
    content: String,
}

pub async fn unified_diff_from_chunks(
    path: &PathUri,
    chunks: &[UpdateFileChunk],
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<ApplyPatchFileUpdate, ApplyPatchError> {
    unified_diff_from_chunks_with_context(path, chunks, /*context*/ 1, fs, sandbox).await
}

pub async fn unified_diff_from_chunks_with_context(
    path: &PathUri,
    chunks: &[UpdateFileChunk],
    context: usize,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<ApplyPatchFileUpdate, ApplyPatchError> {
    derive_new_contents_from_chunks(path, chunks, /*hunk_ordinal*/ None, fs, sandbox)
        .await
        .map(|update| update.into_file_update(context))
}

impl AppliedPatch {
    /// Renders an already derived update without reading its source again.
    fn into_file_update(self, context: usize) -> ApplyPatchFileUpdate {
        let unified_diff = TextDiff::from_lines(&self.original_contents, &self.new_contents)
            .unified_diff()
            .context_radius(context)
            .to_string();
        ApplyPatchFileUpdate {
            unified_diff,
            original_content: self.original_contents,
            content: self.new_contents,
        }
    }
}

/// Print the summary of changes in git-style format.
/// Write a summary of changes to the given writer.
pub fn print_summary(hunks: &[Hunk], out: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(out, "Success. Updated the following files:")?;
    for hunk in hunks {
        match hunk {
            Hunk::AddFile { path, .. } => writeln!(out, "A {}", path.display())?,
            Hunk::DeleteFile { path } => writeln!(out, "D {}", path.display())?,
            Hunk::UpdateFile {
                path, move_path, ..
            } => {
                if let Some(destination) = move_path {
                    writeln!(out, "M {} -> {}", path.display(), destination.display())?;
                } else {
                    writeln!(out, "M {}", path.display())?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn audit147_add_is_create_only_and_rejects_the_entire_stale_proposal() {
        let root = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(root.path()).unwrap();
        fs::write(root.path().join("existing"), "foreign\n").unwrap();
        let error = apply_patch(
            &wrap_patch("*** Add File: prefix\n+never\n*** Add File: existing\n+stale replacement"),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("create-only"));
        assert!(error.delta().is_empty() && error.delta().is_exact());
        assert_eq!(
            fs::read_to_string(root.path().join("existing")).unwrap(),
            "foreign\n"
        );
        assert!(!root.path().join("prefix").exists());
        apply_patch(
            &wrap_patch("*** Add File: absent\n+new"),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("absent")).unwrap(),
            "new\n"
        );
        let hash = hex::encode(Sha256::digest(b"foreign\n"));
        apply_patch(&wrap_patch(&format!("*** Update File: existing\n@@ codex-range 1:1 sha256:{hash}\n+intentional replacement")), &cwd, &mut Vec::new(), &mut Vec::new(), LOCAL_FS.as_ref(), None).await.unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("existing")).unwrap(),
            "intentional replacement\n"
        );
    }
    #[tokio::test]
    async fn audit147_inexact_suffix_does_not_strand_an_independent_owned_prefix() {
        for foreign in [false, true] {
            let root = tempdir().unwrap();
            let prefix = root.path().join("prefix");
            fs::write(&prefix, if foreign { "foreign\n" } else { "owned\n" }).unwrap();
            let uncertain = root.path().join("uncertain");
            fs::write(&uncertain, "external bytes\n").unwrap();
            let mut delta = AppliedPatchDelta::new(
                vec![AppliedPatchChange {
                    path: prefix.clone(),
                    path_uri: PathUri::from_host_native_path(&prefix).unwrap(),
                    move_path_uri: None,
                    change: AppliedPatchFileChange::Add {
                        content: "owned\n".into(),
                        overwritten_content: None,
                    },
                }],
                false,
            );
            assert!(
                rollback_committed_changes(&mut delta, LOCAL_FS.as_ref(), None, None)
                    .await
                    .is_empty()
            );
            assert_eq!(prefix.exists(), foreign);
            if foreign {
                assert_eq!(fs::read_to_string(&prefix).unwrap(), "foreign\n");
            }
            assert_eq!(fs::read_to_string(uncertain).unwrap(), "external bytes\n");
            assert!(!delta.is_exact());
            assert_eq!(delta.changes().len(), usize::from(foreign));
        }
    }
    use super::*;
    use codex_exec_server::LOCAL_FS;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::string::ToString;
    use tempfile::tempdir;

    thread_local! {
        // Count only this derivation's guard/anchor digest, not independent
        // mismatch diagnostics or preflight/execution freshness checks.
        pub(super) static SOURCE_GUARD_HASH_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Helper to construct a patch with the given body.
    fn wrap_patch(body: &str) -> String {
        format!("*** Begin Patch\n{body}\n*** End Patch")
    }

    #[tokio::test]
    async fn move_preserves_source_changed_during_destination_publication() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        fs::write(&source, "before\n").unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let failure = apply_patch_with_cancellation(
            &wrap_patch("*** Update File: source\n*** Move to: destination\n@@\n-before\n+after"),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
            &|| {
                if destination.exists() {
                    fs::write(&source, "external edit\n").unwrap();
                }
                false
            },
        )
        .await
        .unwrap_err();
        assert!(failure.to_string().contains("move source changed"));
        assert_eq!(fs::read_to_string(&source).unwrap(), "external edit\n");
        // Existing rollback owns only the destination; the external edit survives.
        assert!(!destination.exists());
        assert!(failure.delta().is_exact());
        assert!(failure.delta().is_empty());
    }

    #[tokio::test]
    async fn audit147_recovery_path_is_not_rolled_back_even_when_bytes_match() {
        let dir = tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        let recovering = dir.path().join("recovering");
        let changes = [&prefix, &recovering]
            .into_iter()
            .map(|path| {
                fs::write(path, "owned\n").unwrap();
                AppliedPatchChange {
                    path: path.clone(),
                    path_uri: PathUri::from_host_native_path(path).unwrap(),
                    move_path_uri: None,
                    change: AppliedPatchFileChange::Add {
                        content: "owned\n".into(),
                        overwritten_content: None,
                    },
                }
            })
            .collect();
        let failure = anyhow::Error::new(PatchRecoveryRequired {
            path: PathUri::from_host_native_path(&recovering).unwrap(),
            retained_path: "retained-object".into(),
            message: "cleanup failed after publication".into(),
        })
        .context("failed suffix");
        let recovery_path = &failure
            .downcast_ref::<PatchRecoveryRequired>()
            .unwrap()
            .path;
        let mut delta = AppliedPatchDelta::new(changes, false);
        assert!(
            rollback_committed_changes(&mut delta, LOCAL_FS.as_ref(), None, Some(recovery_path))
                .await
                .is_empty()
        );
        assert!(!prefix.exists());
        assert_eq!(fs::read_to_string(&recovering).unwrap(), "owned\n");
        assert!(!delta.is_exact());
        assert_eq!(delta.changes().len(), 1);
    }

    #[tokio::test]
    async fn rollback_keeps_a_move_without_its_destination_uri_as_inexact() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        fs::write(&destination, "after\n").unwrap();
        // An intervening file at the vacated source happens to hold the moved bytes.
        fs::write(&source, "after\n").unwrap();
        let recorded = AppliedPatchChange {
            path: source.clone(),
            path_uri: PathUri::from_host_native_path(&source).unwrap(),
            move_path_uri: None,
            change: AppliedPatchFileChange::Update {
                move_path: Some(destination.clone()),
                old_content: "before\n".into(),
                overwritten_move_content: None,
                new_content: "after\n".into(),
            },
        };
        let mut delta = AppliedPatchDelta::new(vec![recorded.clone()], /*exact*/ true);
        let errors = rollback_committed_changes(&mut delta, LOCAL_FS.as_ref(), None, None).await;
        assert_eq!(errors, Vec::<String>::new());
        assert!(!delta.is_exact());
        assert_eq!(delta.changes(), [recorded].as_slice());
        // Neither side is guessed at: the destination keeps the moved bytes and
        // the source file is not rewritten as if the move were an in-place update.
        assert_eq!(fs::read_to_string(&destination).unwrap(), "after\n");
        assert_eq!(fs::read_to_string(&source).unwrap(), "after\n");
    }

    #[tokio::test]
    async fn preflight_reports_all_conflicts_without_committing_valid_prefix() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "actual a\n").unwrap();
        fs::write(dir.path().join("b.rs"), "actual b\n").unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let patch = wrap_patch(
            "*** Add File: prefix.rs\n+must not exist\n*** Update File: a.rs\n@@\n-wrong a\n+new a\n*** Update File: b.rs\n@@\n-wrong b\n+new b",
        );
        let failure = apply_patch(
            &patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(failure.delta().is_empty());
        assert!(!dir.path().join("prefix.rs").exists());
        let error = failure.to_string();
        assert!(error.contains("a.rs") && error.contains("b.rs"), "{error}");
        assert_eq!(
            fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "actual a\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("b.rs")).unwrap(),
            "actual b\n"
        );
        // The aggregated report keeps the typed mismatches, so a retry receipt
        // can still name the observed source without a separate re-read.
        let (aggregated, _) = failure.into_parts();
        let ApplyPatchError::MultiplePatchConflicts { mismatches, report } = aggregated else {
            panic!("expected retained typed mismatches, got {aggregated:?}");
        };
        assert_eq!(report, error);
        let paths = mismatches
            .iter()
            .map(|mismatch| mismatch.canonical_path.as_str())
            .collect::<Vec<_>>();
        assert!(
            paths.iter().any(|path| path.ends_with("a.rs"))
                && paths.iter().any(|path| path.ends_with("b.rs")),
            "{paths:?}"
        );
        assert!(
            mismatches
                .iter()
                .all(|mismatch| !mismatch.current_content_sha256.is_empty())
        );
    }

    #[tokio::test]
    async fn preflight_reports_each_broken_chunk_once() {
        let dir = tempdir().unwrap();
        let original = "alpha\nbeta\ngamma\ndelta\n";
        fs::write(dir.path().join("a.txt"), original).unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let failure = apply_patch(
            &wrap_patch(
                "*** Update File: a.txt\n@@\n alpha\n-beta\n+BETA\n@@\n-missing-one\n+one\n@@\n-missing-two\n+two\n@@\n-delta\n+DELTA",
            ),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(failure.delta().is_empty());
        let report = failure.to_string();
        // The chunk that failed in sequence is not reported again alone; a
        // later chunk that is broken even alone is its own conflict.
        assert!(report.contains("found 2 conflicts"), "{report}");
        assert_eq!(report.matches("Hunk 1, chunk 2").count(), 1, "{report}");
        assert_eq!(report.matches("Hunk 1, chunk 3").count(), 1, "{report}");
        // Lines absent from the whole file are missing, not misordered.
        assert_eq!(
            report.matches("Failed to find expected lines in").count(),
            2,
            "{report}"
        );
        assert!(!report.contains("file order"), "{report}");
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn later_stale_range_hash_rejects_the_whole_patch_before_writes() {
        let dir = tempdir().unwrap();
        let original = "first\nsecond\n";
        let source = dir.path().join("source.txt");
        fs::write(&source, original).unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let hash = hex::encode(Sha256::digest(original));
        let stale = "0".repeat(64);
        let patch = wrap_patch(&format!(
            "*** Add File: prefix.txt\n+must not be written\n*** Update File: source.txt\n@@ codex-range 1:1 sha256:{hash}\n+changed first\n@@ codex-range 2:2 sha256:{stale}\n+changed second"
        ));
        let failure = apply_patch(
            &patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(failure.delta().is_empty());
        assert!(failure.delta().is_exact());
        let (error, _) = failure.into_parts();
        let ApplyPatchError::PatchContextMismatch(mismatch) = error else {
            panic!("{error:?}");
        };
        assert_eq!(mismatch.kind, PatchContextMismatchKind::StaleRange);
        assert_eq!(mismatch.chunk_ordinal, 2);
        assert_eq!(fs::read_to_string(source).unwrap(), original);
        assert!(!dir.path().join("prefix.txt").exists());
    }

    #[test]
    fn range_source_guard_hash_is_lazy_and_local_to_each_derivation() {
        let path = PathUri::parse("file:///fixture.rs").unwrap();
        for newline in ["\n", "\r\n"] {
            for trailing_newline in [false, true] {
                for count in [1, 4, 16, 64] {
                    let ending = if trailing_newline { newline } else { "" };
                    let original = (0..count)
                        .map(|i| format!("source {i} λ"))
                        .collect::<Vec<_>>()
                        .join(newline)
                        + ending;
                    let expected = (0..count)
                        .map(|i| format!("replacement {i} 漢"))
                        .collect::<Vec<_>>()
                        .join(newline)
                        + ending;
                    let hash = hex::encode(Sha256::digest(original.as_bytes()));
                    let chunks = (0..count)
                        .map(|i| UpdateFileChunk {
                            change_context: Some(format!(
                                "codex-range {}:{} sha256:{hash}",
                                i + 1,
                                i + 1
                            )),
                            old_lines: vec![format!("source {i} λ")],
                            new_lines: vec![format!("replacement {i} 漢")],
                            is_end_of_file: false,
                        })
                        .collect::<Vec<_>>();
                    SOURCE_GUARD_HASH_CALLS.with(|calls| calls.set(0));
                    assert_eq!(
                        new_contents_from_chunks(&path, &original, &chunks, Some(1)).unwrap(),
                        expected
                    );
                    SOURCE_GUARD_HASH_CALLS.with(|calls| assert_eq!(calls.get(), 1));

                    // A later call must not reuse the previous snapshot's hash,
                    // even when the supplied handles and path are unchanged.
                    let changed = original.replacen("source 0", "external edit", 1);
                    SOURCE_GUARD_HASH_CALLS.with(|calls| calls.set(0));
                    let error =
                        new_contents_from_chunks(&path, &changed, &chunks, Some(1)).unwrap_err();
                    let ApplyPatchError::PatchContextMismatch(mismatch) = error else {
                        panic!("{error:?}");
                    };
                    assert_eq!(mismatch.kind, PatchContextMismatchKind::StaleRange);
                    assert_eq!(mismatch.chunk_ordinal, 1);
                    SOURCE_GUARD_HASH_CALLS.with(|calls| assert_eq!(calls.get(), 1));
                }
            }
        }

        let context_only = UpdateFileChunk {
            change_context: None,
            old_lines: vec!["first".into()],
            new_lines: vec!["changed".into()],
            is_end_of_file: false,
        };
        SOURCE_GUARD_HASH_CALLS.with(|calls| calls.set(0));
        assert_eq!(
            new_contents_from_chunks(&path, "first\nsecond\n", &[context_only], Some(1)).unwrap(),
            "changed\nsecond\n"
        );
        SOURCE_GUARD_HASH_CALLS.with(|calls| assert_eq!(calls.get(), 0));
    }

    #[tokio::test]
    async fn revision_bound_ranges_disambiguate_and_reject_stale_source() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.rs");
        let original = "same\r\nsame\r\nlast";
        fs::write(&path, original).unwrap();
        let hash = hex::encode(Sha256::digest(original));
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let patch = wrap_patch(&format!(
            "*** Update File: a.rs\n@@ codex-range 2:2 sha256:{hash}\n+changed"
        ));
        apply_patch(
            &patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "same\r\nchanged\r\nlast"
        );
        let failure = apply_patch(
            &patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(failure.delta().is_empty());
        let (error, _) = failure.into_parts();
        let ApplyPatchError::PatchContextMismatch(mismatch) = error else {
            panic!("{error:?}");
        };
        assert_eq!(mismatch.kind, PatchContextMismatchKind::StaleRange);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "same\r\nchanged\r\nlast"
        );
    }

    #[tokio::test]
    async fn revision_bound_ranges_reject_overlap_and_reverse_order_before_writes() {
        for (first, second, valid) in [
            ("2:3", "3:4", false),
            ("3:4", "1:2", false),
            ("1:2", "3:4", true),
        ] {
            let dir = tempdir().unwrap();
            let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
            let original = "one\ntwo\nthree\nfour\n";
            fs::write(dir.path().join("source.txt"), original).unwrap();
            let hash = hex::encode(Sha256::digest(original));
            let patch = wrap_patch(&format!(
                "*** Add File: prefix.txt\n+prefix\n*** Update File: source.txt\n@@ codex-range {first} sha256:{hash}\n+first replacement\n@@ codex-range {second} sha256:{hash}\n+second replacement"
            ));
            let result = apply_patch(
                &patch,
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None,
            )
            .await;
            if valid {
                result.unwrap();
                assert_eq!(
                    fs::read_to_string(dir.path().join("source.txt")).unwrap(),
                    "first replacement\nsecond replacement\n"
                );
                assert_eq!(
                    fs::read_to_string(dir.path().join("prefix.txt")).unwrap(),
                    "prefix\n"
                );
            } else {
                let failure = result.unwrap_err();
                assert!(failure.delta().is_empty());
                assert!(failure.delta().is_exact());
                let (error, _) = failure.into_parts();
                let ApplyPatchError::PatchContextMismatch(mismatch) = error else {
                    panic!("{error:?}");
                };
                assert_eq!(mismatch.kind, PatchContextMismatchKind::RangeOrderMismatch);
                assert_eq!((mismatch.hunk_ordinal, mismatch.chunk_ordinal), (2, 2));
                assert_eq!(mismatch.current_content_sha256, hash);
                assert_eq!(
                    fs::read_to_string(dir.path().join("source.txt")).unwrap(),
                    original
                );
                assert!(!dir.path().join("prefix.txt").exists());
            }
        }
    }

    #[test]
    fn revision_bound_range_failures_are_distinct_and_located() {
        let original = "first\nsecond\n";
        let lines = original.lines().map(str::to_string).collect::<Vec<_>>();
        let hash = hex::encode(Sha256::digest(original));
        let path = PathUri::parse("file:///fixture.rs").unwrap();
        for (handle, old, kind, located) in [
            (
                "broken".to_string(),
                Vec::new(),
                PatchContextMismatchKind::MalformedRange,
                false,
            ),
            (
                format!("0:2 sha256:{hash}"),
                Vec::new(),
                PatchContextMismatchKind::RangeOutOfBounds,
                false,
            ),
            (
                format!("1:2 sha256:{}", "0".repeat(64)),
                Vec::new(),
                PatchContextMismatchKind::StaleRange,
                true,
            ),
            (
                format!("2:2 sha256:{hash}"),
                vec!["wrong".into()],
                PatchContextMismatchKind::RangeOldLinesMismatch,
                true,
            ),
        ] {
            let chunk = UpdateFileChunk {
                change_context: Some(format!("codex-range {handle}")),
                old_lines: old,
                new_lines: vec!["replacement".into()],
                is_end_of_file: false,
            };
            let error =
                compute_replacements(&lines, original, &path, "fixture.rs", Some(1), &[chunk])
                    .unwrap_err();
            let ApplyPatchError::PatchContextMismatch(mismatch) = error else {
                panic!("typed mismatch");
            };
            assert_eq!(mismatch.kind, kind);
            assert_eq!(mismatch.current_content_sha256, hash);
            assert_eq!(mismatch.current_line_start > 0, located);
            assert_eq!(mismatch.hunk_ordinal, 1);
            assert_eq!(mismatch.chunk_ordinal, 1);
        }
    }

    #[tokio::test]
    async fn ambiguous_matches_return_both_usable_revision_anchors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.rs");
        let original = "first\nold\ncontext\nsecond\nold\ncontext\n";
        fs::write(&path, original).unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let failure = apply_patch(
            &wrap_patch("*** Update File: a.rs\n@@\n-old\n+new\n context"),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(failure.delta().is_empty());
        let (error, _) = failure.into_parts();
        let ApplyPatchError::PatchContextMismatch(mismatch) = error else {
            panic!("{error:?}");
        };
        let hash = hex::encode(Sha256::digest(original));
        let anchor = format!("@@ codex-range 5:6 sha256:{hash}");
        assert!(
            mismatch
                .message
                .contains(&format!("@@ codex-range 2:3 sha256:{hash}"))
        );
        assert!(mismatch.message.contains(&anchor));
        assert!(mismatch.message.contains("first") && mismatch.message.contains("second"));
        apply_patch(
            &wrap_patch(&format!("*** Update File: a.rs\n{anchor}\n+new\n+context")),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "first\nold\ncontext\nsecond\nnew\ncontext\n"
        );
    }

    #[tokio::test]
    async fn revision_bound_replacements_match_contextual_bytes_and_reject_stale_revisions() {
        for count in [64, 256, 1024] {
            for newline in ["\n", "\r\n"] {
                for trailing_newline in [false, true] {
                    let dir = tempdir().unwrap();
                    let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
                    let removed = (0..count)
                        .map(|index| format!("    old_{index}(\"λ 日本語\");"))
                        .collect::<Vec<_>>();
                    let replacement = (0..count)
                        .map(|index| format!("    new_{index}(\"β 日本語\");"))
                        .collect::<Vec<_>>();
                    let source = format!(
                        "prefix{newline}{}{newline}suffix{}",
                        removed.join(newline),
                        if trailing_newline { newline } else { "" },
                    );
                    let expected = format!(
                        "prefix{newline}{}{newline}suffix{}",
                        replacement.join(newline),
                        if trailing_newline { newline } else { "" },
                    );
                    let hash = hex::encode(Sha256::digest(source.as_bytes()));
                    let added = replacement
                        .iter()
                        .map(|line| format!("+{line}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let ordinary = wrap_patch(&format!(
                        "*** Update File: contextual.rs\n@@\n prefix\n{}{added}\n suffix",
                        removed
                            .iter()
                            .map(|line| format!("-{line}\n"))
                            .collect::<String>(),
                    ));
                    let compact = wrap_patch(&format!(
                        "*** Update File: ranged.rs\n@@ codex-range 2:{} sha256:{hash}\n{added}",
                        count + 1,
                    ));
                    for (name, patch) in [("contextual.rs", &ordinary), ("ranged.rs", &compact)] {
                        fs::write(dir.path().join(name), &source).unwrap();
                        apply_patch(
                            patch,
                            &cwd,
                            &mut Vec::new(),
                            &mut Vec::new(),
                            LOCAL_FS.as_ref(),
                            None,
                        )
                        .await
                        .unwrap();
                        assert_eq!(
                            fs::read(dir.path().join(name)).unwrap(),
                            expected.as_bytes()
                        );
                    }
                    // Even an edit outside the selected range invalidates the
                    // whole-file identity. Never silently accept a stale view.
                    let drifted = source.replace("suffix", "user change outside selection");
                    let path = dir.path().join("ranged.rs");
                    fs::write(&path, &drifted).unwrap();
                    let error = apply_patch(
                        &compact,
                        &cwd,
                        &mut Vec::new(),
                        &mut Vec::new(),
                        LOCAL_FS.as_ref(),
                        None,
                    )
                    .await
                    .unwrap_err();
                    assert!(error.delta().is_empty());
                    assert_eq!(fs::read(path).unwrap(), drifted.as_bytes());
                }
            }
        }
    }

    #[tokio::test]
    async fn blank_source_lines_and_eof_are_literal_preconditions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        for (contents, body) in [
            ("alpha\nbeta\n", "@@\n-\n+replacement"),
            ("alpha\nbeta\n", "@@\n-"),
            ("alpha\nbeta\n", "@@\n-\n+replacement\n*** End of File"),
            ("alpha\n\n", "@@\n-\n-\n+replacement"),
            ("alpha\nbeta\n", "@@ alpha\n+replacement\n*** End of File"),
        ] {
            fs::write(&path, contents).unwrap();
            let failure = apply_patch(
                &wrap_patch(&format!("*** Update File: sample.txt\n{body}")),
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .unwrap_err();
            assert!(failure.delta().is_empty());
            assert_eq!(fs::read_to_string(&path).unwrap(), contents);
            // The patch itself is well formed; only the source precondition fails.
            let (error, _) = failure.into_parts();
            let ApplyPatchError::PatchContextMismatch(mismatch) = error else {
                panic!("{body:?}: {error:?}");
            };
            assert_eq!(
                mismatch.kind,
                PatchContextMismatchKind::ExpectedLinesNotFound,
                "{body:?}"
            );
        }
        for (contents, body, expected) in [
            ("alpha\n\n", "@@\n-\n+replacement", "alpha\nreplacement\n"),
            (
                "alpha\nbeta\n",
                "@@ beta\n+replacement\n*** End of File",
                "alpha\nbeta\nreplacement\n",
            ),
        ] {
            fs::write(&path, contents).unwrap();
            apply_patch(
                &wrap_patch(&format!("*** Update File: sample.txt\n{body}")),
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn patch_input_limit_rejects_oversize_before_writes() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, "keep me\n").unwrap();
        let mut patch = wrap_patch("*** Delete File: a.txt");
        // Trailing whitespace is valid, but it still counts toward the input bound.
        patch.extend(std::iter::repeat_n(
            ' ',
            parser::MAX_PATCH_INPUT_BYTES - patch.len(),
        ));
        assert_eq!(parse_patch(&patch).unwrap().hunks.len(), 1);
        patch.push(' ');
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = apply_patch(
            &patch,
            &cwd,
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        let (error, delta) = error.into_parts();
        assert!(delta.is_empty());
        assert_eq!(
            error,
            ApplyPatchError::ParseError(ParseError::InvalidPatchError(format!(
                "PATCH input exceeds the {}-byte limit",
                parser::MAX_PATCH_INPUT_BYTES
            )))
        );
        assert!(stdout.is_empty());
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("PATCH input exceeds")
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "keep me\n");
    }

    #[tokio::test]
    async fn located_patch_failures_report_snapshot_without_writing() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let path = dir.path().join("a.txt");
        for (original, body, kind, diagnostic) in [
            (
                "before\nanchor\none\nafter\n",
                "@@\n anchor\n-stale\n+new\n after",
                PatchContextMismatchKind::ExpectedLinesNotFound,
                "Failed to find expected lines",
            ),
            (
                "prefix\n  old\n",
                "@@\n-old\n+new",
                PatchContextMismatchKind::IndentationMismatch,
                "removed line 2",
            ),
        ] {
            fs::write(&path, original).unwrap();
            let failure = apply_patch(
                &wrap_patch(&format!("*** Update File: a.txt\n{body}")),
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .unwrap_err();
            assert!(failure.delta().is_empty());
            assert!(failure.delta().is_exact());
            let (error, _) = failure.into_parts();
            let ApplyPatchError::PatchContextMismatch(mismatch) = error else {
                panic!("{error:?}");
            };
            assert_eq!(mismatch.kind, kind);
            assert_eq!((mismatch.hunk_ordinal, mismatch.chunk_ordinal), (1, 1));
            assert_eq!(
                mismatch.current_content_sha256,
                hex::encode(Sha256::digest(original.as_bytes()))
            );
            assert_eq!(mismatch.current_line_start, 1);
            assert_eq!(mismatch.current_line_end, original.lines().count());
            assert!(
                mismatch.message.contains(diagnostic),
                "{}",
                mismatch.message
            );
            assert!(mismatch.current_excerpt.contains("1 |"));
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[tokio::test]
    async fn out_of_order_chunks_apply_where_each_site_is_unique() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let path = dir.path().join("a.txt");
        for (original, body, expected) in [
            (
                "first\nlast\n",
                "@@\n-last\n+LAST\n@@\n-first\n+FIRST",
                "FIRST\nLAST\n",
            ),
            // The relocated chunk inserts where the earlier-listed chunk's
            // replacement starts; the insertion still lands before it.
            ("a\nb\n", "@@\n-b\n+B\n@@\n a\n+inserted", "a\ninserted\nB\n"),
            (
                "fn one() {\n}\nfn two() {\n}\n",
                "@@ fn two() {\n+    two();\n@@ fn one() {\n+    one();",
                "fn one() {\n    one();\n}\nfn two() {\n    two();\n}\n",
            ),
        ] {
            fs::write(&path, original).unwrap();
            apply_patch(
                &wrap_patch(&format!("*** Update File: a.txt\n{body}")),
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), expected, "{body}");
        }
    }

    #[tokio::test]
    async fn out_of_order_chunks_reject_ambiguous_or_overlapping_sites() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let path = dir.path().join("a.txt");
        for (original, body, kind, diagnostics) in [
            (
                "first\nfirst\nlast\n",
                "@@\n-last\n+LAST\n@@\n-first\n+FIRST",
                PatchContextMismatchKind::AmbiguousMatch,
                [
                    "after line 3",
                    "they match at lines 1 and 2",
                    "must match one location",
                ],
            ),
            (
                "a\nb\nc\n",
                "@@\n b\n-c\n+C\n@@\n-a\n+A\n b",
                PatchContextMismatchKind::ExpectedLinesNotFound,
                [
                    "after line 3",
                    "overlaps chunk 1 at lines 2-3",
                    "Merge the two chunks",
                ],
            ),
        ] {
            fs::write(&path, original).unwrap();
            let failure = apply_patch(
                &wrap_patch(&format!("*** Update File: a.txt\n{body}")),
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .unwrap_err();
            assert!(failure.delta().is_empty());
            let ApplyPatchError::PatchContextMismatch(mismatch) = failure.into_parts().0 else {
                panic!("expected a located mismatch for {body}");
            };
            assert_eq!(mismatch.kind, kind);
            assert_eq!(mismatch.chunk_ordinal, 2);
            assert!(mismatch.current_line_start > 0, "{}", mismatch.message);
            for diagnostic in diagnostics {
                assert!(
                    mismatch.message.contains(diagnostic),
                    "{diagnostic}: {}",
                    mismatch.message
                );
            }
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[tokio::test]
    async fn later_chunks_select_an_ambiguous_first_chunk_or_list_every_candidate() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let path = dir.path().join("a.txt");
        let apply = |body: &str| {
            let patch = wrap_patch(&format!("*** Update File: a.txt\n{body}"));
            let cwd = cwd.clone();
            async move {
                apply_patch(
                    &patch,
                    &cwd,
                    &mut Vec::new(),
                    &mut Vec::new(),
                    LOCAL_FS.as_ref(),
                    None,
                )
                .await
            }
        };
        for (original, body, expected) in [
            // Only the first `value();` is followed by the second chunk.
            (
                "fn one() {\n    value();\n    first_only();\n}\nfn two() {\n    value();\n}\n",
                "@@\n-    value();\n+    changed();\n@@\n-    first_only();\n+    renamed();",
                "fn one() {\n    changed();\n    renamed();\n}\nfn two() {\n    value();\n}\n",
            ),
            // Either site writes the same bytes.
            ("x\nx\nx\n", "@@\n-x\n x", "x\nx\n"),
        ] {
            fs::write(&path, original).unwrap();
            apply(body).await.unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), expected, "{body}");
        }

        // Two of three candidates fit the rest of the hunk but differ.
        let original = "v();\nw();\nv();\nw();\nv();\nend();\n";
        fs::write(&path, original).unwrap();
        let failure = apply("@@\n-v();\n+V();\n@@\n-w();\n+W();")
            .await
            .unwrap_err();
        assert!(failure.delta().is_empty());
        let ApplyPatchError::PatchContextMismatch(mismatch) = failure.into_parts().0 else {
            panic!("expected an ambiguous match");
        };
        assert_eq!(mismatch.kind, PatchContextMismatchKind::AmbiguousMatch);
        let hash = hex::encode(Sha256::digest(original));
        for diagnostic in [
            "Ambiguous exact match at lines 1, 3 and 5".to_string(),
            "Only lines 1 and 3 let this chunk and the rest of this hunk apply".to_string(),
            format!("@@ codex-range 1:1 sha256:{hash}"),
            format!("@@ codex-range 3:3 sha256:{hash}"),
            format!("@@ codex-range 5:5 sha256:{hash}"),
        ] {
            assert!(
                mismatch.message.contains(&diagnostic),
                "{diagnostic}: {}",
                mismatch.message
            );
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn missing_lines_report_the_closest_current_region() {
        let path = PathUri::parse("file:///fixture.rs").unwrap();
        let mismatch = |original: &str, old: &[&str], new: &[&str]| {
            let chunk = UpdateFileChunk {
                change_context: None,
                old_lines: old.iter().map(ToString::to_string).collect(),
                new_lines: new.iter().map(ToString::to_string).collect(),
                is_end_of_file: false,
            };
            match new_contents_from_chunks(&path, original, &[chunk], Some(1)).unwrap_err() {
                ApplyPatchError::PatchContextMismatch(mismatch) => mismatch,
                error => panic!("{error:?}"),
            }
        };

        // The call is wrapped, with a trailing comma, in the current source.
        let wrapped = mismatch(
            "fn check() {\n    compare_field(\n        &mut issues,\n        \"toolchain\",\n    );\n    done();\n}\n",
            &["    compare_field(&mut issues, \"toolchain\");"],
            &["    compare_field(&mut issues, \"host\");"],
        );
        assert!(
            wrapped
                .message
                .contains("(current lines 2-5 hold the same code formatted differently"),
            "{}",
            wrapped.message
        );
        assert_eq!(wrapped.kind, PatchContextMismatchKind::ExpectedLinesNotFound);
        assert!(wrapped.current_excerpt.contains("\"toolchain\","));
        // The expected lines still follow the first line break of the message.
        assert!(
            wrapped
                .message
                .ends_with(":\n    compare_field(&mut issues, \"toolchain\");")
        );

        // Another writer already made this change.
        let applied = mismatch(
            "fn test() {\n    let value = super::capture(&snapshot).unwrap();\n}\n",
            &["    let value = capture(&snapshot).unwrap();"],
            &["    let value = super::capture(&snapshot).unwrap();"],
        );
        assert!(
            applied.message.contains(
                "(the chunk's replacement lines already match current line 2; the change may already be applied)"
            ),
            "{}",
            applied.message
        );
        assert_eq!(
            (applied.current_line_start, applied.current_line_end),
            (1, 3)
        );

        // No line matches at any tier, but one is clearly the closest.
        let similar = mismatch(
            "fn test() {\n    let value = super::capture(&snapshot).unwrap();\n    finish();\n}\n",
            &["    let value = capture(&snapshot).unwrap();"],
            &["    let value = capture(&snapshot).expect(\"captured\");"],
        );
        assert!(
            similar
                .message
                .contains("(line 2 is the closest current line and is shown below)"),
            "{}",
            similar.message
        );
        assert!(similar.current_excerpt.contains("super::capture"));

        // Nothing is close: no region is guessed.
        let unrelated = mismatch(
            "alpha\nbeta\n",
            &["completely different text"],
            &["replacement"],
        );
        assert_eq!(unrelated.current_line_start, 0);
        assert!(!unrelated.message.contains(" ("), "{}", unrelated.message);
    }

    #[test]
    fn long_chunk_mismatch_excerpt_is_located_in_a_medium_file() {
        // One expected line differs among many; the old per-line scan exceeded
        // its comparison budget on a file of this size and returned nothing.
        let lines = (0..3000)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>();
        let mut expected = lines[1000..1080].to_vec();
        expected[40] = "stale copy".to_string();
        let chunk = UpdateFileChunk {
            change_context: None,
            old_lines: expected,
            new_lines: vec![],
            is_end_of_file: false,
        };
        let (start, end, excerpt) =
            bounded_patch_mismatch_excerpt(&lines, &chunk).expect("located mismatch");
        assert!(start <= 1041 && end >= 1041, "{start}-{end}");
        assert!(excerpt.contains("line 1040"), "{excerpt}");
    }

    #[tokio::test]
    async fn repeated_updates_of_one_file_apply_in_sequence() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let path = dir.path().join("a.txt");
        let original = "one\ntwo\nthree\n";
        fs::write(&path, original).unwrap();
        // The second update edits the first update's result.
        let delta = apply_patch(
            &wrap_patch(
                "*** Update File: a.txt\n@@\n-one\n+ONE\n*** Add File: b.txt\n+b\n*** Update File: ./a.txt\n@@\n-three\n+THREE\n@@\n-ONE\n+uno",
            ),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "uno\ntwo\nTHREE\n");
        assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "b\n");
        let updates = delta
            .changes()
            .iter()
            .filter_map(|change| match &change.change {
                AppliedPatchFileChange::Update {
                    old_content,
                    new_content,
                    ..
                } => Some((old_content.as_str(), new_content.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            updates,
            [
                (original, "ONE\ntwo\nthree\n"),
                ("ONE\ntwo\nthree\n", "uno\ntwo\nTHREE\n"),
            ]
        );

        // A later update that does not fit the earlier result writes nothing.
        let failure = apply_patch(
            &wrap_patch(
                "*** Update File: a.txt\n@@\n-uno\n+one\n*** Add File: c.txt\n+c\n*** Update File: a.txt\n@@\n-uno\n+again",
            ),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(failure.delta().is_empty());
        let ApplyPatchError::PatchContextMismatch(mismatch) = failure.into_parts().0 else {
            panic!("expected a located mismatch");
        };
        assert_eq!((mismatch.hunk_ordinal, mismatch.chunk_ordinal), (3, 1));
        assert_eq!(mismatch.edits_result_of_hunk, Some(1));
        assert!(
            mismatch
                .message
                .contains("This hunk edits the result of hunk 1 for the same file"),
            "{}",
            mismatch.message
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "uno\ntwo\nTHREE\n");
        assert!(!dir.path().join("c.txt").exists());
    }

    #[tokio::test]
    async fn repeated_json_updates_are_validated_from_first_source_to_last_result() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let path = dir.path().join("a.json");
        // The intermediate `[1,]` is not JSON, but the sequence's result is.
        fs::write(&path, "[\n1\n]\n").unwrap();
        apply_patch(
            &wrap_patch(
                "*** Update File: a.json\n@@\n-1\n+1,\n*** Update File: a.json\n@@\n 1,\n+2",
            ),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "[\n1,\n2\n]\n");

        // A sequence that ends invalid is rejected before any write.
        let failure = apply_patch(
            &wrap_patch(
                "*** Update File: a.json\n@@\n-2\n+3\n*** Update File: a.json\n@@\n-3\n+3,",
            ),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(failure.delta().is_empty());
        assert!(
            failure.to_string().contains("candidate is invalid"),
            "{failure}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "[\n1,\n2\n]\n");
    }

    #[tokio::test]
    async fn test_add_fixture_replays_recorded_contents() {
        let tmp = tempdir().unwrap();
        let path = PathUri::from_host_native_path(tmp.path().join("a.txt")).unwrap();
        for (content, expected) in [
            ("", ""),
            ("hello", "hello\n"),
            ("a\nb\n\n", "a\nb\n\n"),
            ("x\r\n", "x\r\n"),
        ] {
            let action = ApplyPatchAction::new_add_for_test(&path, content.to_string());
            assert_eq!(
                action.changes().get(&path),
                Some(&ApplyPatchFileChange::Add {
                    content: expected.to_string()
                })
            );
            apply_patch(
                &action.patch,
                &action.cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn missing_delete_rejects_before_any_write() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let failure = apply_patch(
            &wrap_patch("*** Add File: created.txt\n+created\n*** Delete File: absent.txt"),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            failure.to_string().contains("Failed to delete file"),
            "{failure}"
        );
        assert!(failure.delta().is_exact());
        assert!(failure.delta().is_empty());
        assert!(!dir.path().join("created.txt").exists());
        assert!(!dir.path().join("absent.txt").exists());
        assert!(
            !failure
                .delta()
                .failure_summary()
                .contains("Additional filesystem changes")
        );
    }

    #[tokio::test]
    async fn test_legacy_mismatch_expected_lines_are_bounded() {
        let tmp = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(tmp.path()).unwrap();
        fs::write(tmp.path().join("a.txt"), "unrelated\n").unwrap();
        for expected in [
            vec!["\u{754c}".repeat(PATCH_MISMATCH_MAX_BYTES)],
            vec!["absent".to_string(); 100],
        ] {
            let body = expected
                .iter()
                .map(|line| format!("-{line}\n"))
                .collect::<String>();
            let patch =
                format!("*** Begin Patch\n*** Update File: a.txt\n@@\n{body}+new\n*** End Patch");
            let mut stderr = Vec::new();
            let failure = apply_patch(
                &patch,
                &cwd,
                &mut Vec::new(),
                &mut stderr,
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .unwrap_err();
            assert!(failure.delta().is_empty());
            let (error, _) = failure.into_parts();
            let ApplyPatchError::PatchContextMismatch(mismatch) = error else {
                panic!("{error:?}")
            };
            assert_eq!(
                mismatch.kind,
                PatchContextMismatchKind::ExpectedLinesNotFound
            );
            assert_eq!(mismatch.current_line_start, 0);
            let (_, excerpt) = mismatch.message.split_once(":\n").unwrap();
            assert!(excerpt.len() <= PATCH_MISMATCH_MAX_BYTES);
            assert!(excerpt.lines().count() <= PATCH_MISMATCH_MAX_LINES + 1);
            assert!(excerpt.ends_with("[expected lines truncated; reread the full target region]"));
            assert_eq!(
                fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
                "unrelated\n"
            );
        }
    }

    #[tokio::test]
    async fn patch_context_mismatch_reports_preflight_identity_and_recovers() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).expect("absolute test path");
        let path = dir.path().join("sample.txt");
        fs::write(&path, "alpha\nanchor\nstale-current\nomega\n").unwrap();
        let patch = wrap_patch(
            "*** Add File: prefix.txt\n+committed\n*** Update File: sample.txt\n@@\n anchor\n-stale-old\n+fixed\n omega",
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let failure = apply_patch(
            &patch,
            &cwd,
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .expect_err("second hunk should mismatch");
        let (error, delta) = failure.into_parts();
        let ApplyPatchError::PatchContextMismatch(mismatch) = error else {
            panic!("expected structured mismatch");
        };
        let post_prefix_contents = "alpha\nanchor\nstale-current\nomega\n";
        assert!(!dir.path().join("prefix.txt").exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), post_prefix_contents);
        assert!(delta.is_empty());
        assert_eq!(mismatch.hunk_ordinal, 2);
        assert_eq!(mismatch.chunk_ordinal, 1);
        assert_eq!(
            mismatch.canonical_path,
            PathUri::from_host_native_path(&path).unwrap().to_string()
        );
        assert_eq!(
            mismatch.current_content_sha256,
            hex::encode(Sha256::digest(post_prefix_contents.as_bytes()))
        );
        assert!(mismatch.current_excerpt.contains("alpha"));
        assert!(mismatch.current_excerpt.contains("stale-current"));
        assert_eq!(
            (mismatch.current_line_start, mismatch.current_line_end),
            (1, 4)
        );
        assert!(mismatch.current_excerpt.len() <= PATCH_MISMATCH_MAX_BYTES);
        assert!(mismatch.current_line_end - mismatch.current_line_start < PATCH_MISMATCH_MAX_LINES);
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains("PatchContextMismatch:"), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "Hunk {}, chunk {}",
                mismatch.hunk_ordinal, mismatch.chunk_ordinal
            )),
            "{stderr}"
        );
        assert!(
            stderr.contains(&mismatch.current_excerpt),
            "excerpt must retain real line breaks: {stderr}"
        );

        let corrected = wrap_patch(
            "*** Add File: prefix.txt\n+committed\n*** Update File: sample.txt\n@@\n anchor\n-stale-current\n+fixed\n omega",
        );
        apply_patch(
            &corrected,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .expect("fresh returned context should support a corrected patch");
        assert_eq!(
            fs::read_to_string(dir.path().join("prefix.txt")).unwrap(),
            "committed\n"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "alpha\nanchor\nfixed\nomega\n"
        );
    }

    #[test]
    fn ambiguous_patch_context_preserves_legacy_failure() {
        let lines = vec![
            "anchor".to_string(),
            "current".to_string(),
            "anchor".to_string(),
            "current".to_string(),
        ];
        let chunk = UpdateFileChunk {
            change_context: None,
            old_lines: vec!["anchor".to_string(), "stale".to_string()],
            new_lines: vec!["anchor".to_string(), "new".to_string()],
            is_end_of_file: false,
        };

        assert_eq!(bounded_patch_mismatch_excerpt(&lines, &chunk), None);
    }

    #[test]
    fn tied_mismatch_shows_the_candidate_that_keeps_the_leading_anchor() {
        // An edit inserted a test between the chunk's anchor and its target, so
        // the anchor site and the target site each match two of three lines.
        let lines = [
            "mod tests {",
            "    static LOCK: Mutex<()> = Mutex::new(());",
            "",
            "    #[test]",
            "    fn inserted() {}",
            "",
            "    fn target() {}",
            "}",
        ]
        .map(String::from)
        .to_vec();
        let chunk = UpdateFileChunk {
            change_context: None,
            old_lines: vec![
                "    static LOCK: Mutex<()> = Mutex::new(());".to_string(),
                String::new(),
                "    fn target() {}".to_string(),
            ],
            new_lines: vec![],
            is_end_of_file: false,
        };

        let (start, end, excerpt) =
            bounded_patch_mismatch_excerpt(&lines, &chunk).expect("anchored candidate");
        // The anchored candidate first differs at line 4, shown with three
        // context lines either side. The other tied candidate would give 2-8
        // and the single-line fallback at the anchor 1-5.
        assert_eq!((start, end), (1, 7));
        assert!(excerpt.contains("static LOCK"), "{excerpt}");
        assert!(excerpt.contains("#[test]"), "{excerpt}");
    }

    #[test]
    fn mismatch_excerpt_preserves_blank_offsets_and_shows_late_discrepancy() {
        for mut lines in [
            vec!["anchor".to_string(), String::new(), "old".to_string()],
            (0..40).map(|index| format!("line-{index}")).collect(),
        ] {
            let expected = lines.clone();
            let changed = lines.len() - 2;
            lines[changed] = "actual-discrepancy".to_string();
            let chunk = UpdateFileChunk {
                change_context: None,
                old_lines: expected,
                new_lines: vec![],
                is_end_of_file: false,
            };
            let (start, end, excerpt) = bounded_patch_mismatch_excerpt(&lines, &chunk).unwrap();
            assert!(start <= changed + 1 && end > changed);
            assert!(excerpt.contains("actual-discrepancy"));
        }
    }

    #[test]
    fn mismatch_excerpt_uses_matching_tiers_then_change_context() {
        let lines = ["prefix", "    fn target() {", "        actual();", "    }"]
            .map(String::from)
            .to_vec();
        for chunk in [
            UpdateFileChunk {
                change_context: None,
                old_lines: vec!["fn target() {".into(), "missing();".into()],
                new_lines: vec![],
                is_end_of_file: false,
            },
            UpdateFileChunk {
                change_context: Some("fn target() {".into()),
                old_lines: vec!["missing();".into()],
                new_lines: vec![],
                is_end_of_file: false,
            },
        ] {
            let (_, _, excerpt) = bounded_patch_mismatch_excerpt(&lines, &chunk)
                .expect("unique whitespace-tolerant anchor");
            assert!(excerpt.contains("actual();"), "{excerpt}");
        }
    }

    #[test]
    fn unavailable_excerpt_preserves_failure_identity() {
        let lines = vec!["unrelated".repeat(4 * 1024 * 1024)];
        let path =
            PathUri::from_host_native_path(std::env::current_dir().unwrap().join("file.txt"))
                .unwrap();
        let chunk = UpdateFileChunk {
            change_context: None,
            old_lines: vec!["missing".to_string()],
            new_lines: vec![],
            is_end_of_file: false,
        };
        assert!(bounded_patch_mismatch_excerpt(&lines, &chunk).is_none());
        let error = patch_context_mismatch(
            PatchMismatchSource {
                original_lines: &lines,
                original_contents: &lines[0],
                path: &path,
                hunk_ordinal: Some(2),
                source_hash: None,
            },
            3,
            &chunk,
            PatchContextMismatchKind::ExpectedLinesNotFound,
            "missing source".to_string(),
        );
        let ApplyPatchError::PatchContextMismatch(mismatch) = error else {
            panic!("lost failure identity")
        };
        assert_eq!(mismatch.hunk_ordinal, 2);
        assert_eq!(mismatch.chunk_ordinal, 3);
        assert_eq!(
            mismatch.kind,
            PatchContextMismatchKind::ExpectedLinesNotFound
        );
        assert_eq!(
            mismatch.current_content_sha256,
            hex::encode(Sha256::digest(lines[0].as_bytes()))
        );
        assert!(mismatch.to_string().contains("excerpt unavailable"));
    }

    #[test]
    fn patch_context_mismatch_excerpt_is_utf8_and_byte_bounded() {
        // Three-byte characters: the 4064 bytes left for this line after the
        // anchor line and both prefixes end inside a character, so truncation
        // has to back up to a boundary instead of cutting at the budget.
        let long_line = "\u{754c}".repeat(PATCH_MISMATCH_MAX_BYTES);
        let lines = vec![
            "unique-anchor".to_string(),
            long_line,
            "current-tail".to_string(),
        ];
        let chunk = UpdateFileChunk {
            change_context: None,
            old_lines: vec!["unique-anchor".to_string(), "stale-tail".to_string()],
            new_lines: vec!["unique-anchor".to_string(), "fixed-tail".to_string()],
            is_end_of_file: false,
        };

        let (start, end, excerpt) =
            bounded_patch_mismatch_excerpt(&lines, &chunk).expect("unique mismatch location");
        // The excerpt stops in the mismatched line, kept up to the last whole
        // character that fits the budget.
        assert_eq!((start, end), (1, 2));
        assert!(excerpt.len() <= PATCH_MISMATCH_MAX_BYTES);
        assert!(excerpt.len() > PATCH_MISMATCH_MAX_BYTES - '\u{754c}'.len_utf8());
        assert!(excerpt.ends_with('\u{754c}'));
    }

    #[tokio::test]
    async fn test_add_file_hunk_creates_file_with_contents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("add.txt");
        let patch = wrap_patch(&format!(
            r#"*** Add File: {}
+ab
+cd"#,
            path.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        // Verify expected stdout and stderr outputs.
        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nA {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");
        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(contents, "ab\ncd\n");
    }

    #[tokio::test]
    async fn test_apply_patch_hunks_accept_relative_and_absolute_paths() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).expect("absolute test path");
        let relative_add = dir.path().join("relative-add.txt");
        let absolute_add = dir.path().join("absolute-add.txt");
        let relative_delete = dir.path().join("relative-delete.txt");
        let absolute_delete = dir.path().join("absolute-delete.txt");
        let relative_update = dir.path().join("relative-update.txt");
        let absolute_update = dir.path().join("absolute-update.txt");
        fs::write(&relative_delete, "delete relative\n").unwrap();
        fs::write(&absolute_delete, "delete absolute\n").unwrap();
        fs::write(&relative_update, "relative old\n").unwrap();
        fs::write(&absolute_update, "absolute old\n").unwrap();

        let patch = wrap_patch(&format!(
            r#"*** Add File: relative-add.txt
+relative add
*** Add File: {}
+absolute add
*** Delete File: relative-delete.txt
*** Delete File: {}
*** Update File: relative-update.txt
@@
-relative old
+relative new
*** Update File: {}
@@
-absolute old
+absolute new"#,
            absolute_add.display(),
            absolute_delete.display(),
            absolute_update.display(),
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        apply_patch(
            &patch,
            &cwd,
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();

        assert_eq!(fs::read_to_string(&relative_add).unwrap(), "relative add\n");
        assert_eq!(fs::read_to_string(&absolute_add).unwrap(), "absolute add\n");
        assert!(!relative_delete.exists());
        assert!(!absolute_delete.exists());
        assert_eq!(
            fs::read_to_string(&relative_update).unwrap(),
            "relative new\n"
        );
        assert_eq!(
            fs::read_to_string(&absolute_update).unwrap(),
            "absolute new\n"
        );
        assert_eq!(String::from_utf8(stderr).unwrap(), "");
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!(
                "Success. Updated the following files:\nA relative-add.txt\nA {}\nD relative-delete.txt\nD {}\nM relative-update.txt\nM {}\n",
                absolute_add.display(),
                absolute_delete.display(),
                absolute_update.display(),
            )
        );
    }

    #[tokio::test]
    async fn delete_rejects_source_changed_after_preflight_and_restores_prefix() {
        for changed in [b"concurrent\r\nedit\n".as_slice(), b"\xff\xfe".as_slice()] {
            let dir = tempdir().unwrap();
            let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
            let source = dir.path().join("source.txt");
            let prefix = dir.path().join("prefix.txt");
            fs::write(&source, "original\n").unwrap();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let failure = apply_patch_with_cancellation(
                &wrap_patch("*** Add File: prefix.txt\n+committed\n*** Delete File: source.txt\n*** Add File: suffix.txt\n+not committed"),
                &cwd,
                &mut stdout,
                &mut stderr,
                LOCAL_FS.as_ref(),
                None,
                &|| {
                    // The real publication boundary runs after global preflight.
                    if prefix.exists() {
                        fs::write(&source, changed).unwrap();
                    }
                    false
                },
            )
            .await
            .expect_err("an intervening edit must not be deleted");
            assert_eq!(fs::read(&source).unwrap(), changed);
            assert!(!prefix.exists());
            assert!(!dir.path().join("suffix.txt").exists());
            assert!(stdout.is_empty());
            assert!(failure.delta().is_exact());
            assert!(failure.delta().is_empty());
            if std::str::from_utf8(changed).is_ok() {
                assert!(failure.to_string().contains("refusing to delete"));
            }
            assert!(
                !String::from_utf8(stderr)
                    .unwrap()
                    .contains("do not retry the whole patch")
            );
        }
    }

    #[tokio::test]
    async fn test_delete_file_hunk_removes_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("del.txt");
        fs::write(&path, "x").unwrap();
        let patch = wrap_patch(&format!("*** Delete File: {}", path.display()));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let delta = apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        assert!(delta.is_exact());
        assert_eq!(
            delta.changes,
            vec![AppliedPatchChange {
                path: path.clone(),
                path_uri: PathUri::from_host_native_path(&path).unwrap(),
                move_path_uri: None,
                change: AppliedPatchFileChange::Delete {
                    content: "x".into()
                },
            }]
        );
        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nD {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_update_file_hunk_modifies_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("update.txt");
        fs::write(&path, "foo\nbar\n").unwrap();
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
 foo
-bar
+baz"#,
            path.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        // Validate modified file contents and expected stdout/stderr.
        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nM {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "foo\nbaz\n");
    }

    #[tokio::test]
    async fn json_update_rejects_invalid_candidate_before_any_write() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).expect("absolute test path");
        let broken = dir.path().join("catalog.json");
        let valid = dir.path().join("settings.json");
        let jsonc = dir.path().join("tsconfig.json");
        fs::write(&broken, "{\n  \"a\": 1,\n  \"b\": 2\n}\n").unwrap();
        fs::write(&valid, "{\n  \"a\": 1\n}\n").unwrap();
        fs::write(&jsonc, "{\n  // comment\n  \"a\": 1\n}\n").unwrap();
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
   "a": 1,
-  "b": 2
+  "b": 2,
 }}
*** Update File: {}
@@
-  "a": 1
+  "a": 2
*** Update File: {}
@@
-  "a": 1
+  "a": 2"#,
            broken.display(),
            valid.display(),
            jsonc.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &cwd,
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap_err();
        assert!(stdout.is_empty());
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains("candidate is invalid"), "{stderr}");
        assert!(stderr.contains("line 4"), "{stderr}");
        assert_eq!(fs::read_to_string(&valid).unwrap(), "{\n  \"a\": 1\n}\n");
        assert_eq!(
            fs::read_to_string(&jsonc).unwrap(),
            "{\n  // comment\n  \"a\": 1\n}\n"
        );
        assert_eq!(
            fs::read_to_string(&broken).unwrap(),
            "{\n  \"a\": 1,\n  \"b\": 2\n}\n"
        );
        let repaired = patch.replace("+  \"b\": 2,", "+  \"b\": 3");
        apply_patch(
            &repaired,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(fs::read_to_string(&valid).unwrap(), "{\n  \"a\": 2\n}\n");
        assert_eq!(
            fs::read_to_string(&jsonc).unwrap(),
            "{\n  // comment\n  \"a\": 2\n}\n"
        );
    }

    #[tokio::test]
    async fn json_update_revalidates_after_concurrent_edit() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let source = dir.path().join("source.json");
        let prefix = dir.path().join("prefix.txt");
        let changed = "{\n\"key\":\n1\n}\n";
        fs::write(&source, "[\n1\n]\n").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let failure = apply_patch_with_cancellation(
            &wrap_patch(
                "*** Add File: prefix.txt\n+committed\n*** Update File: source.json\n@@\n-1\n+2,3\n*** Add File: suffix.txt\n+not committed",
            ),
            &cwd,
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            None,
            &|| {
                // Preflight accepts [2,3], but the same replacement would
                // invalidate the object written after the prefix commits.
                if prefix.exists() {
                    fs::write(&source, changed).unwrap();
                }
                false
            },
        )
        .await
        .expect_err("a refreshed JSON candidate must be validated before writing");
        assert!(failure.to_string().contains("candidate is invalid"));
        assert_eq!(fs::read_to_string(&source).unwrap(), changed);
        assert!(!prefix.exists());
        assert!(!dir.path().join("suffix.txt").exists());
        assert!(stdout.is_empty());
        assert!(failure.delta().is_exact());
        assert!(failure.delta().is_empty());
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(!stderr.contains("do not retry the whole patch"), "{stderr}");
        assert!(!stderr.contains("No files were changed"), "{stderr}");
    }

    #[tokio::test]
    async fn test_update_preserves_line_endings_and_final_newline() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let path = dir.path().join("update.txt");
        for (original, body, expected) in [
            (
                "foo\r\nbar\r\nend\r\n",
                "@@\n-bar\n+baz",
                "foo\r\nbaz\r\nend\r\n",
            ),
            ("foo\r\nbar", "@@\n-bar\n+baz", "foo\r\nbaz"),
            ("foo\nbar", "@@\n-bar\n+baz", "foo\nbaz"),
            (
                "foo\r\nbar\nend\r\n",
                "@@\n-foo\n+new",
                "new\r\nbar\nend\r\n",
            ),
            ("foo", "@@\n+bar", "foo\nbar"),
            ("foo\r\nbar", "@@\n+baz", "foo\r\nbar\r\nbaz"),
            ("foo", "@@\n-foo", ""),
            ("", "@@\n+foo", "foo\n"),
            ("foo\n\n", "@@\n-foo\n+bar", "bar\n\n"),
        ] {
            fs::write(&path, original).unwrap();
            let patch = wrap_patch(&format!("*** Update File: update.txt\n{body}"));
            apply_patch(
                &patch,
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                fs::read(&path).unwrap(),
                expected.as_bytes(),
                "original: {original:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_update_file_hunk_can_move_file() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dst.txt");
        fs::write(&src, "line\n").unwrap();
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
*** Move to: {}
@@
-line
+line2"#,
            src.display(),
            dest.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        // Validate move semantics and expected stdout/stderr.
        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nM {} -> {}\n",
            src.display(),
            dest.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");
        assert!(!src.exists());
        let contents = fs::read_to_string(&dest).unwrap();
        assert_eq!(contents, "line2\n");
    }

    #[tokio::test]
    async fn test_rename_only_hunk_preserves_contents_exactly() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).expect("absolute test path");

        for (name, contents) in [
            ("no-final-newline", "contents without newline"),
            ("multiple-final-newlines", "contents with newlines\n\n\n"),
        ] {
            let src_name = format!("{name}-src.txt");
            let dest_name = format!("{name}-dest.txt");
            let src = dir.path().join(&src_name);
            let dest = dir.path().join(&dest_name);
            fs::write(&src, contents).unwrap();
            let patch = wrap_patch(&format!(
                "*** Update File: {src_name}\n*** Move to: {dest_name}"
            ));
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            apply_patch(
                &patch,
                &cwd,
                &mut stdout,
                &mut stderr,
                LOCAL_FS.as_ref(),
                /*sandbox*/ None,
            )
            .await
            .unwrap();

            assert!(!src.exists());
            assert_eq!(fs::read(&dest).unwrap(), contents.as_bytes());
            assert!(stderr.is_empty());
        }
    }

    /// Verify that a single `Update File` hunk with multiple change chunks can update different
    /// parts of a file and that the file is listed only once in the summary.
    #[tokio::test]
    async fn test_multiple_update_chunks_apply_to_single_file() {
        // Start with a file containing four lines.
        let dir = tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        fs::write(&path, "foo\nbar\nbaz\nqux\n").unwrap();
        // Construct an update patch with two separate change chunks.
        // The first chunk uses the line `foo` as context and transforms `bar` into `BAR`.
        // The second chunk uses `baz` as context and transforms `qux` into `QUX`.
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
 foo
-bar
+BAR
@@
 baz
-qux
+QUX"#,
            path.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nM {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "foo\nBAR\nbaz\nQUX\n");
    }

    /// A more involved `Update File` hunk that exercises additions, deletions and
    /// replacements in separate chunks that appear in non‑adjacent parts of the
    /// file.  Verifies that all edits are applied and that the summary lists the
    /// file only once.
    #[tokio::test]
    async fn test_update_file_hunk_interleaved_changes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("interleaved.txt");

        // Original file: six numbered lines.
        fs::write(&path, "a\nb\nc\nd\ne\nf\n").unwrap();

        // Patch performs:
        //  • Replace `b` → `B`
        //  • Replace `e` → `E` (using surrounding context)
        //  • Append new line `g` at the end‑of‑file
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
 a
-b
+B
@@
 c
 d
-e
+E
@@
 f
+g
*** End of File"#,
            path.display()
        ));

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();

        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();

        let expected_out = format!(
            "Success. Updated the following files:\nM {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");

        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "a\nB\nc\nd\nE\nf\ng\n");
    }

    #[tokio::test]
    async fn test_pure_addition_chunk_followed_by_removal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("panic.txt");
        fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
+after-context
+second-line
@@
 line1
-line2
-line3
+line2-replacement"#,
            path.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(
            contents,
            "line1\nline2-replacement\nafter-context\nsecond-line\n"
        );
    }

    /// Exact current Unicode removals remain supported. Normalized context may
    /// locate a hunk, but must not authorize destroying different current bytes.
    #[tokio::test]
    async fn test_update_line_with_unicode_dash() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("unicode.py");

        // Original line contains EN DASH (\u{2013}) and NON-BREAKING HYPHEN (\u{2011}).
        let original = "import asyncio  # local import \u{2013} avoids top\u{2011}level dep\n";
        std::fs::write(&path, original).unwrap();

        // Remove the actual current Unicode line, not an ASCII approximation.
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
-import asyncio  # local import – avoids top‑level dep
+import asyncio  # HELLO"#,
            path.display()
        ));

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();

        // File should now contain the replaced comment.
        let expected = "import asyncio  # HELLO\n";
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, expected);

        // Ensure success summary lists the file as modified.
        let stdout_str = String::from_utf8(stdout).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nM {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);

        // No stderr expected.
        assert_eq!(String::from_utf8(stderr).unwrap(), "");
    }

    #[tokio::test]
    async fn unicode_space_matching_preserves_nested_content_and_rejects_ambiguity() {
        for ambiguous in [false, true] {
            let dir = tempdir().unwrap();
            let path = dir.path().join("nested.py");
            let original = if ambiguous {
                "if enabled:\n    if nested:\n        label = \"a\u{00a0}b\"\n    label = \"a\u{00a0}b\"\n"
            } else {
                "if enabled:\n    if nested:\n        label = \"a\u{00a0}b\"\n    finish()\n"
            };
            std::fs::write(&path, original).unwrap();
            let patch = wrap_patch(&format!(
                "*** Update File: {}\n@@\n-        label = \"a b\"\n+        label = \"updated\"",
                path.display()
            ));
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let result = apply_patch(
                &patch,
                &PathUri::from_host_native_path(dir.path()).unwrap(),
                &mut stdout,
                &mut stderr,
                LOCAL_FS.as_ref(),
                /*sandbox*/ None,
            )
            .await;
            let contents = std::fs::read_to_string(&path).unwrap();
            if ambiguous {
                let failure = result.unwrap_err();
                assert!(failure.delta().is_empty());
                assert!(failure.to_string().contains("Ambiguous Unicode-normalized"));
                assert_eq!(contents, original);
                assert!(stdout.is_empty());
            } else {
                let failure = result.unwrap_err();
                assert!(failure.delta().is_empty());
                assert!(
                    failure
                        .to_string()
                        .contains("normalization is only allowed for unchanged context")
                );
                assert_eq!(contents, original);
                assert!(stdout.is_empty());
            }
        }
    }

    // Child of the regression below; ordinary discovery does no work.
    #[tokio::test]
    async fn audit107_patch_transaction_child() {
        let Some(root) = std::env::var_os("KD4_AUDIT107_TRANSACTION_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let cwd = PathUri::from_host_native_path(&root).unwrap();
        let args = parse_patch(&wrap_patch(
            "*** Update File: source\n*** Move to: destination",
        ))
        .unwrap();
        let _transaction = transaction::acquire(&args.hunks, &cwd, LOCAL_FS.as_ref(), None)
            .await
            .unwrap()
            .unwrap();
        fs::write(root.join("admitted"), b"ready").unwrap();
        std::future::pending::<()>().await;
    }

    #[tokio::test]
    async fn audit107_patch_transactions_exclude_endpoints_and_release_on_death() {
        use std::time::Duration;
        let root = tempdir().unwrap();
        fs::write(root.path().join("source"), "own = 1\nother = 7\n").unwrap();
        fs::write(root.path().join("independent"), "before\n").unwrap();
        let cwd = PathUri::from_host_native_path(root.path()).unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::audit107_patch_transaction_child",
                "--nocapture",
            ])
            .env("KD4_AUDIT107_TRANSACTION_ROOT", root.path())
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !root.path().join("admitted").exists() {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "child exited before admission"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        for body in [
            "*** Update File: source\n@@\n-own = 1\n+own = 2",
            "*** Delete File: source",
            "*** Add File: source\n+overwrite",
            "*** Update File: source\n*** Move to: new-target",
            "*** Update File: independent\n*** Move to: destination",
            "*** Add File: destination\n+overwrite",
            "*** Add File: prefix\n+must not commit\n*** Update File: source\n@@\n-own = 1\n+own = 2",
        ] {
            let failure = tokio::time::timeout(
                Duration::from_secs(2),
                apply_patch(
                    &wrap_patch(body),
                    &cwd,
                    &mut Vec::new(),
                    &mut Vec::new(),
                    LOCAL_FS.as_ref(),
                    None,
                ),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert!(failure.delta().is_empty());
            assert!(failure.to_string().contains("busy"), "{failure}");
            assert_eq!(
                fs::read_to_string(root.path().join("source")).unwrap(),
                "own = 1\nother = 7\n"
            );
            assert!(!root.path().join("destination").exists());
            assert!(!root.path().join("prefix").exists());
        }
        // Independent endpoints continue while the other process owns its pair.
        apply_patch(
            &wrap_patch("*** Update File: independent\n@@\n-before\n+after"),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap();
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        apply_patch(
            &wrap_patch("*** Update File: source\n@@\n-own = 1\n+own = 2"),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("source")).unwrap(),
            "own = 2\nother = 7\n"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("independent")).unwrap(),
            "after\n"
        );
    }

    #[tokio::test]
    async fn audit107_textual_anchor_cannot_relocate_into_a_sibling() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("module.py");
        let original = "def target():\n    return 3\n\ndef sibling():\n    return 1\n";
        fs::write(&path, original).unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let patch = wrap_patch(
            "*** Add File: prefix\n+must not commit\n*** Update File: module.py\n@@ def target():\n-    return 1\n+    return 2",
        );
        let failure = apply_patch(
            &patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(failure.delta().is_empty());
        assert!(
            failure
                .to_string()
                .contains("no later sibling was searched")
        );
        assert!(!dir.path().join("prefix").exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let ApplyPatchError::PatchContextMismatch(mismatch) = failure.into_parts().0 else {
            panic!("expected a located anchor conflict");
        };
        assert!(mismatch.current_excerpt.contains("return 3"));

        // Rebinding the intended, fully inspected target cannot relocate.
        let hash = hex::encode(Sha256::digest(original.as_bytes()));
        let patch = wrap_patch(&format!(
            "*** Update File: module.py\n@@ codex-range 1:2 sha256:{hash}\n+def target():\n+    return 2"
        ));
        apply_patch(
            &patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "def target():\n    return 2\n\ndef sibling():\n    return 1\n"
        );
    }

    #[tokio::test]
    async fn audit107_removed_bytes_are_exact_but_equal_context_remains_tolerant() {
        for (old, current) in [
            ("label = \"fast-mode\"", "label = \"fast–mode\""),
            ("label = \"mode\"", "label = “mode”"),
            ("label = \"a b\"", "label = \"a\u{00a0}b\""),
            ("label = \"mode\"", "label = \"mode\"  "),
        ] {
            for ending in ["\n", "\r\n"] {
                let dir = tempdir().unwrap();
                let path = dir.path().join("value.txt");
                let original = format!("{current}{ending}value = 1{ending}");
                fs::write(&path, &original).unwrap();
                let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
                let stale = wrap_patch(&format!(
                    "*** Update File: value.txt\n@@\n-{old}\n+label = \"safe-mode\""
                ));
                let failure = apply_patch(
                    &stale,
                    &cwd,
                    &mut Vec::new(),
                    &mut Vec::new(),
                    LOCAL_FS.as_ref(),
                    None,
                )
                .await
                .unwrap_err();
                assert!(failure.delta().is_empty());
                let ApplyPatchError::PatchContextMismatch(mismatch) = failure.into_parts().0 else {
                    panic!("expected a located removed-line mismatch");
                };
                assert_eq!(mismatch.kind, PatchContextMismatchKind::RemovedLineMismatch);
                assert_eq!(fs::read_to_string(&path).unwrap(), original);
                let safe = wrap_patch(&format!(
                    "*** Update File: value.txt\n@@\n {old}\n-value = 1\n+value = 2"
                ));
                apply_patch(
                    &safe,
                    &cwd,
                    &mut Vec::new(),
                    &mut Vec::new(),
                    LOCAL_FS.as_ref(),
                    None,
                )
                .await
                .unwrap();
                assert_eq!(
                    fs::read_to_string(&path).unwrap(),
                    format!("{current}{ending}value = 2{ending}")
                );
            }
        }
    }

    #[tokio::test]
    async fn chunks_after_an_earlier_match_take_the_nearest_candidate() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("arms.rs");
        let original = "match code {\n    1 => (\n        \"cancelled\",\n    ),\n    _ => (\n        \"deadline\",\n    ),\n}\n";
        std::fs::write(&path, original).unwrap();
        // Chunk 2 repeats a line that occurs twice after chunk 1; sequential
        // chunks select the first occurrence after the preceding match.
        let patch = wrap_patch(&format!(
            "*** Update File: {}\n@@\n     1 => (\n@@\n     ),\n+    3 => (\n+        \"memory\",\n+    ),",
            path.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).unwrap(),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "match code {\n    1 => (\n        \"cancelled\",\n    ),\n    3 => (\n        \"memory\",\n    ),\n    _ => (\n        \"deadline\",\n    ),\n}\n"
        );

        // The first chunk has no preceding match, so its location must be unique.
        std::fs::write(&path, original).unwrap();
        let patch = wrap_patch(&format!(
            "*** Update File: {}\n@@\n     ),\n+    3 => (),",
            path.display()
        ));
        let failure = apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).unwrap(),
            &mut Vec::<u8>::new(),
            &mut Vec::<u8>::new(),
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("Ambiguous exact match at lines 4 and 7"),
            "{failure}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        // When a chunk fails, re-checking chunks alone must not report the
        // anchored chunk as ambiguous. Only chunks after the failing one are
        // re-checked, so the anchored chunk is placed on either side of it.
        for chunks in [
            "@@\n     1 => (\n@@\n     ),\n+    3 => (),\n@@\n-    missing => (),",
            "@@\n     1 => (\n@@\n-    missing => (),\n@@\n     ),\n+    3 => (),",
        ] {
            let patch = wrap_patch(&format!("*** Update File: {}\n{chunks}", path.display()));
            let failure = apply_patch(
                &patch,
                &PathUri::from_host_native_path(dir.path()).unwrap(),
                &mut Vec::<u8>::new(),
                &mut Vec::<u8>::new(),
                LOCAL_FS.as_ref(),
                /*sandbox*/ None,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(
                failure.contains("Failed to find expected lines"),
                "{failure}"
            );
            assert!(!failure.contains("Ambiguous"), "{failure}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[tokio::test]
    async fn test_unified_diff() {
        // Start with a file containing four lines.
        let dir = tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        fs::write(&path, "foo\nbar\nbaz\nqux\n").unwrap();
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
 foo
-bar
+BAR
@@
 baz
-qux
+QUX"#,
            path.display()
        ));
        let patch = parse_patch(&patch).unwrap();

        let update_file_chunks = match patch.hunks.as_slice() {
            [Hunk::UpdateFile { chunks, .. }] => chunks,
            _ => panic!("Expected a single UpdateFile hunk"),
        };
        let path_uri = PathUri::from_host_native_path(&path).expect("absolute test path");
        let diff = unified_diff_from_chunks(
            &path_uri,
            update_file_chunks,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let expected_diff = r#"@@ -1,4 +1,4 @@
 foo
-bar
+BAR
 baz
-qux
+QUX
"#;
        let expected = ApplyPatchFileUpdate {
            unified_diff: expected_diff.to_string(),
            original_content: "foo\nbar\nbaz\nqux\n".to_string(),
            content: "foo\nBAR\nbaz\nQUX\n".to_string(),
        };
        assert_eq!(expected, diff);
    }

    #[tokio::test]
    async fn test_unified_diff_first_line_replacement() {
        // Replace the very first line of the file.
        let dir = tempdir().unwrap();
        let path = dir.path().join("first.txt");
        fs::write(&path, "foo\nbar\nbaz\n").unwrap();

        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
-foo
+FOO
 bar"#,
            path.display()
        ));

        let patch = parse_patch(&patch).unwrap();
        let chunks = match patch.hunks.as_slice() {
            [Hunk::UpdateFile { chunks, .. }] => chunks,
            _ => panic!("Expected a single UpdateFile hunk"),
        };

        let resolved_path = PathUri::from_host_native_path(&path).expect("absolute test path");
        let diff = unified_diff_from_chunks(
            &resolved_path,
            chunks,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let expected_diff = r#"@@ -1,2 +1,2 @@
-foo
+FOO
 bar
"#;
        let expected = ApplyPatchFileUpdate {
            unified_diff: expected_diff.to_string(),
            original_content: "foo\nbar\nbaz\n".to_string(),
            content: "FOO\nbar\nbaz\n".to_string(),
        };
        assert_eq!(expected, diff);
    }

    #[tokio::test]
    async fn test_unified_diff_last_line_replacement() {
        // Replace the very last line of the file.
        let dir = tempdir().unwrap();
        let path = dir.path().join("last.txt");
        fs::write(&path, "foo\nbar\nbaz\n").unwrap();

        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
 foo
 bar
-baz
+BAZ"#,
            path.display()
        ));

        let patch = parse_patch(&patch).unwrap();
        let chunks = match patch.hunks.as_slice() {
            [Hunk::UpdateFile { chunks, .. }] => chunks,
            _ => panic!("Expected a single UpdateFile hunk"),
        };

        let resolved_path = PathUri::from_host_native_path(&path).expect("absolute test path");
        let diff = unified_diff_from_chunks(
            &resolved_path,
            chunks,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let expected_diff = r#"@@ -2,2 +2,2 @@
 bar
-baz
+BAZ
"#;
        let expected = ApplyPatchFileUpdate {
            unified_diff: expected_diff.to_string(),
            original_content: "foo\nbar\nbaz\n".to_string(),
            content: "foo\nbar\nBAZ\n".to_string(),
        };
        assert_eq!(expected, diff);
    }

    #[tokio::test]
    async fn test_unified_diff_insert_at_eof() {
        // Insert a new line at end‑of‑file.
        let dir = tempdir().unwrap();
        let path = dir.path().join("insert.txt");
        fs::write(&path, "foo\nbar\nbaz\n").unwrap();

        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
+quux
*** End of File
"#,
            path.display()
        ));

        let patch = parse_patch(&patch).unwrap();
        let chunks = match patch.hunks.as_slice() {
            [Hunk::UpdateFile { chunks, .. }] => chunks,
            _ => panic!("Expected a single UpdateFile hunk"),
        };

        let path_uri = PathUri::from_host_native_path(&path).expect("absolute test path");
        let diff =
            unified_diff_from_chunks(&path_uri, chunks, LOCAL_FS.as_ref(), /*sandbox*/ None)
                .await
                .unwrap();
        let expected_diff = r#"@@ -3 +3,2 @@
 baz
+quux
"#;
        let expected = ApplyPatchFileUpdate {
            unified_diff: expected_diff.to_string(),
            original_content: "foo\nbar\nbaz\n".to_string(),
            content: "foo\nbar\nbaz\nquux\n".to_string(),
        };
        assert_eq!(expected, diff);
    }

    #[tokio::test]
    async fn test_unified_diff_interleaved_changes() {
        // Original file with six lines.
        let dir = tempdir().unwrap();
        let path = dir.path().join("interleaved.txt");
        fs::write(&path, "a\nb\nc\nd\ne\nf\n").unwrap();

        // Patch replaces two separate lines and appends a new one at EOF using
        // three distinct chunks.
        let patch_body = format!(
            r#"*** Update File: {}
@@
 a
-b
+B
@@
 d
-e
+E
@@
 f
+g
*** End of File"#,
            path.display()
        );
        let patch = wrap_patch(&patch_body);

        // Extract chunks then build the unified diff.
        let parsed = parse_patch(&patch).unwrap();
        let chunks = match parsed.hunks.as_slice() {
            [Hunk::UpdateFile { chunks, .. }] => chunks,
            _ => panic!("Expected a single UpdateFile hunk"),
        };

        let path_uri = PathUri::from_host_native_path(&path).expect("absolute test path");
        let diff =
            unified_diff_from_chunks(&path_uri, chunks, LOCAL_FS.as_ref(), /*sandbox*/ None)
                .await
                .unwrap();

        let expected_diff = r#"@@ -1,6 +1,7 @@
 a
-b
+B
 c
 d
-e
+E
 f
+g
"#;

        let expected = ApplyPatchFileUpdate {
            unified_diff: expected_diff.to_string(),
            original_content: "a\nb\nc\nd\ne\nf\n".to_string(),
            content: "a\nB\nc\nd\nE\nf\ng\n".to_string(),
        };

        assert_eq!(expected, diff);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(
            contents,
            r#"a
B
c
d
E
f
g
"#
        );
    }

    #[tokio::test]
    async fn test_failure_summary_survives_cancellation_and_output_failure() {
        struct BrokenOutput;
        impl io::Write for BrokenOutput {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "output closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        for cancel in [false, true] {
            let dir = tempdir().unwrap();
            let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
            let created = dir.path().join("created.txt");
            let mut stderr = Vec::new();
            let failure = apply_patch_with_cancellation(
                &wrap_patch("*** Add File: created.txt\n+created\n*** Add File: later.txt\n+later"),
                &cwd,
                &mut BrokenOutput,
                &mut stderr,
                LOCAL_FS.as_ref(),
                None,
                &|| cancel && created.exists(),
            )
            .await
            .unwrap_err();
            let stderr = String::from_utf8(stderr).unwrap();
            assert!(failure.delta().is_exact());
            if cancel {
                assert!(!created.exists());
                assert!(failure.delta().is_empty());
            } else {
                assert_eq!(fs::read_to_string(&created).unwrap(), "created\n");
            }
            assert_eq!(dir.path().join("later.txt").exists(), !cancel);
            assert_eq!(
                stderr.matches(&format!("A {}", created.display())).count(),
                usize::from(!cancel)
            );
            assert_eq!(
                stderr.contains(&format!("A {}", dir.path().join("later.txt").display())),
                !cancel
            );
            assert_eq!(
                stderr.contains("do not retry the whole patch"),
                !cancel,
                "{stderr}"
            );
            assert!(
                stderr.contains(if cancel { "cancelled" } else { "output closed" }),
                "{stderr}"
            );
        }
    }

    #[tokio::test]
    async fn test_unreadable_destinations_are_not_overwritten() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("binary.dat");
        fs::write(dir.path().join("source.txt"), "before\n").unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).expect("absolute test path");

        for patch in [
            wrap_patch("*** Add File: binary.dat\n+text"),
            wrap_patch("*** Update File: source.txt\n*** Move to: binary.dat\n@@\n-before\n+after"),
        ] {
            fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let failure = apply_patch(
                &patch,
                &cwd,
                &mut stdout,
                &mut stderr,
                LOCAL_FS.as_ref(),
                /*sandbox*/ None,
            )
            .await
            .unwrap_err();

            assert!(failure.delta().is_exact());
            assert!(failure.delta().is_empty());
            assert!(stdout.is_empty());
            assert_eq!(fs::read(&path).unwrap(), [0xff, 0xfe, 0xfd]);
            assert_eq!(
                fs::read_to_string(dir.path().join("source.txt")).unwrap(),
                "before\n"
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn read_denied_destinations_are_not_overwritten() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempdir().unwrap();
        let destination = dir.path().join("destination.txt");
        let source = dir.path().join("source.txt");
        fs::write(&source, "before\n").unwrap();
        fs::write(&destination, "keep\n").unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        // Permit writes and replacement, but deny reads. The preimage check,
        // not a coincidental write failure, must prevent mutation.
        let denied = fs::OpenOptions::new()
            .write(true)
            .share_mode(2 | 4)
            .open(&destination)
            .unwrap();
        for patch in [
            wrap_patch("*** Add File: destination.txt\n+replacement"),
            wrap_patch(
                "*** Update File: source.txt\n*** Move to: destination.txt\n@@\n-before\n+after",
            ),
        ] {
            let failure = apply_patch(
                &patch,
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .unwrap_err();
            assert!(failure.delta().is_exact());
            assert!(failure.delta().is_empty());
        }
        drop(denied);
        assert_eq!(fs::read_to_string(destination).unwrap(), "keep\n");
        assert_eq!(fs::read_to_string(source).unwrap(), "before\n");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn failed_write_restores_prefix_when_destination_is_unchanged() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("locked.txt");
        fs::write(&path, "before\n").unwrap();
        let _reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ, deliberately disallow writes.
            .open(&path)
            .unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let mut stderr = Vec::new();
        let failure = apply_patch(
            &wrap_patch(
                "*** Add File: prefix.txt\n+committed\n*** Update File: locked.txt\n@@\n-before\n+after\n*** Add File: suffix.txt\n+not committed",
            ),
            &cwd,
            &mut Vec::new(),
            &mut stderr,
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(fs::read_to_string(&path).unwrap(), "before\n");
        assert!(!dir.path().join("prefix.txt").exists());
        assert!(!dir.path().join("suffix.txt").exists());
        assert!(failure.delta().is_exact());
        assert!(failure.delta().is_empty());
        let summary = String::from_utf8(stderr).unwrap();
        assert!(!summary.contains("prefix.txt"));
        assert!(
            summary.contains("os error 32") || summary.contains("os error 5"),
            "{summary}"
        );
        assert!(!summary.contains("Additional filesystem changes"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn multi_file_failure_restores_all_originals_and_identical_patch_retries() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempdir().unwrap();
        let originals = [
            ("update", "old\r\n"),
            ("delete", "deleted\n"),
            ("move", "source\n"),
            ("destination", "overwritten\n"),
            ("add", "replaced\n"),
            ("locked", "locked\n"),
        ];
        for (name, bytes) in originals {
            fs::write(dir.path().join(name), bytes).unwrap();
        }
        let reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(dir.path().join("locked"))
            .unwrap();
        let patch = wrap_patch(
            "*** Update File: update\n@@\n-old\n+new\n*** Delete File: delete\n*** Update File: move\n*** Move to: destination\n@@\n-source\n+moved\n*** Add File: add\n+replacement\n*** Add File: new\n+created\n*** Update File: locked\n@@\n-locked\n+unlocked",
        );
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let failure = apply_patch(
            &patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(failure.delta().is_empty());
        assert!(failure.delta().is_exact());
        for (name, bytes) in originals {
            assert_eq!(fs::read_to_string(dir.path().join(name)).unwrap(), bytes);
        }
        assert!(!dir.path().join("new").exists());
        drop(reader);
        apply_patch(
            &patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("update")).unwrap(),
            "new\r\n"
        );
        assert!(!dir.path().join("delete").exists());
        assert!(!dir.path().join("move").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("destination")).unwrap(),
            "moved\n"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn rollback_failure_reports_remaining_changes_without_overwriting_external_edits() {
        use std::os::windows::fs::OpenOptionsExt;
        for external_edit in [false, true] {
            let dir = tempdir().unwrap();
            let prefix = dir.path().join("prefix");
            let locked = dir.path().join("locked");
            fs::write(&locked, "before\n").unwrap();
            let _reader = fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(&locked)
                .unwrap();
            let prefix_reader = std::sync::Mutex::new(None);
            let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
            let failure = apply_patch_with_cancellation(
                &wrap_patch("*** Add File: prefix\n+committed\n*** Update File: locked\n@@\n-before\n+after"),
                &cwd, &mut Vec::new(), &mut Vec::new(), LOCAL_FS.as_ref(), None,
                &|| {
                    if prefix.exists() {
                        if external_edit { fs::write(&prefix, "external\n").unwrap(); }
                        *prefix_reader.lock().unwrap() = Some(fs::OpenOptions::new().read(true).share_mode(1).open(&prefix).unwrap());
                    }
                    false
                },
            ).await.unwrap_err();
            assert_eq!(failure.delta().changes().len(), 1);
            assert_eq!(failure.delta().is_exact(), !external_edit);
            assert_eq!(
                fs::read_to_string(&prefix).unwrap(),
                if external_edit {
                    "external\n"
                } else {
                    "committed\n"
                }
            );
            assert!(
                failure
                    .delta()
                    .failure_summary()
                    .contains("do not retry the whole patch")
            );
        }
    }
}
