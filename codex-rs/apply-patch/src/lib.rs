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
use codex_utils_path_uri::PathUri;
use codex_utils_path_uri::PathUriParseError;
pub use parser::Hunk;
pub use parser::ParseError;
use parser::ParseError::*;
pub use parser::UpdateFileChunk;
pub use parser::UpdateLineKind;
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
    // Boxed so every `Result<AppliedPatchDelta, ApplyPatchFailure>` stays small.
    delta: Box<AppliedPatchDelta>,
}

impl ApplyPatchFailure {
    fn new(error: ApplyPatchError, delta: AppliedPatchDelta) -> Self {
        Self {
            error,
            delta: Box::new(delta),
        }
    }

    fn without_delta(error: ApplyPatchError) -> Self {
        Self::new(error, AppliedPatchDelta::empty())
    }

    pub fn delta(&self) -> &AppliedPatchDelta {
        &self.delta
    }

    pub fn into_parts(self) -> (ApplyPatchError, AppliedPatchDelta) {
        (self.error, *self.delta)
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

#[expect(
    clippy::too_many_arguments,
    reason = "Patch application threads output sinks, filesystem, sandbox, and cancellation"
)]
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
                        "delete source changed after verification; refusing to delete {path}");
                    let index = hunks.iter().position(|hunk| hunk.resolve_path(cwd).ok().as_ref() == Some(path))
                        .context("verified delete is missing from patch")?;
                    anyhow::ensure!(prepared[index].as_ref().is_some_and(|source| source.original_contents == *content),
                        "delete source changed during preparation; refusing to delete {path}");
                }
            }
            ensure_rebased_updates_keep_their_sites(hunks, cwd, &previous_updates, &prepared, action)?;
        }
        apply_hunks_to_files(hunks, cwd, fs, sandbox, &mut delta, prepared, &destinations, is_cancelled, verified.is_some()).await
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
#[expect(
    clippy::too_many_arguments,
    reason = "Patch application threads filesystem, sandbox, prepared state, and cancellation"
)]
async fn apply_hunks_to_files(
    hunks: &[Hunk],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    delta: &mut AppliedPatchDelta,
    mut prepared: Vec<Option<AppliedPatch>>,
    destinations: &HashMap<PathUri, Option<String>>,
    is_cancelled: &(dyn Fn() -> bool + Sync),
    verified_sites: bool,
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
        anyhow::ensure!(!verified_sites,
            "source changed after verified edit-site preparation; refusing to reselect an update in {}",
            path.inferred_native_path_string());
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
                    anyhow::ensure!(!verified_sites,
                        "source changed after verified edit-site preparation; refusing to reselect an update in {}",
                        path_uri.inferred_native_path_string());
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

impl<'a> PreparedSource<'a> {
    fn new(contents: &'a str) -> Self {
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

fn chunk_edit_operations(chunk: &UpdateFileChunk, pattern: &[String]) -> Result<Vec<similar::DiffOp>, ApplyPatchError> {
    use parser::UpdateLineKind;
    if chunk.line_kinds.is_empty() {
        return Ok(similar::capture_diff_slices(similar::Algorithm::RawMyers, pattern, &chunk.new_lines));
    }
    let (mut old, mut new) = (0, 0);
    let mut group: Option<(usize, usize)> = None;
    let mut operations = Vec::new();
    for kind in chunk.line_kinds.iter().copied().chain(std::iter::once(UpdateLineKind::Context)) {
        match kind {
            UpdateLineKind::Context => {
                if let Some((old_index, new_index)) = group.take() {
                    let (old_len, new_len) = (old - old_index, new - new_index);
                    if old > pattern.len() || new > chunk.new_lines.len() {
                        return Err(ApplyPatchError::ComputeReplacements("invalid patch line provenance".into()));
                    }
                    operations.push(match (old_len, new_len) {
                        (0, _) => similar::DiffOp::Insert { old_index, new_index, new_len },
                        (_, 0) => similar::DiffOp::Delete { old_index, old_len, new_index },
                        _ => similar::DiffOp::Replace { old_index, old_len, new_index, new_len },
                    });
                }
                old += 1; new += 1;
            }
            UpdateLineKind::Add => { group.get_or_insert((old, new)); new += 1; }
            UpdateLineKind::Remove => { group.get_or_insert((old, new)); old += 1; }
        }
    }
    if old != chunk.old_lines.len() + 1 || new != chunk.new_lines.len() + 1 {
        return Err(ApplyPatchError::ComputeReplacements("invalid patch line provenance".into()));
    }
    Ok(operations)
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
            for operation in chunk_edit_operations(chunk, pattern)? {
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
        .await?
        .into_file_update(context)
        .await
}

impl AppliedPatch {
    /// Renders an already derived update without reading its source again.
    async fn into_file_update(
        self,
        context: usize,
    ) -> std::result::Result<ApplyPatchFileUpdate, ApplyPatchError> {
        // Small previews retain their existing algorithm and avoid a worker
        // handoff. Large comparisons must not monopolize an async polling
        // thread or multiply CPU/memory pressure across concurrent patches.
        const OFFLOAD_BYTES: usize = 256 * 1024;
        static RENDER_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
        if self
            .original_contents
            .len()
            .saturating_add(self.new_contents.len())
            < OFFLOAD_BYTES
        {
            return Ok(self.render_file_update(context, false));
        }
        let permit = RENDER_SLOTS.acquire().await.map_err(|error| {
            ApplyPatchError::ComputeReplacements(format!("patch preview unavailable: {error}"))
        })?;
        tokio::task::spawn_blocking(move || {
            // Cancellation of the waiter must not free a CPU slot while its
            // read-only renderer is still running. No filesystem work is done
            // here; all source identity checks and mutation stay with callers.
            let _permit = permit;
            self.render_file_update(context, true)
        })
        .await
        .map_err(|error| {
            ApplyPatchError::ComputeReplacements(format!("patch preview failed: {error}"))
        })
    }

    fn render_file_update(self, context: usize, bounded: bool) -> ApplyPatchFileUpdate {
        let mut config = TextDiff::configure();
        if bounded {
            // Bounds search, not formatting: similar falls back to a coarser
            // complete edit script, retaining every old/new byte for approval.
            // Authoritative inserted-line diagnostics do not use this preview.
            config.timeout(std::time::Duration::from_millis(100));
        }
        let unified_diff = config
            .diff_lines(&self.original_contents, &self.new_contents)
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
