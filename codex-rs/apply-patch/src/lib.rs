mod invocation;
mod parser;
mod seek_sequence;
mod standalone_executable;
mod streaming_parser;

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
pub use invocation::maybe_parse_apply_patch_verified;
pub use invocation::maybe_parse_apply_patch_verified_for_environment;
pub use invocation::verify_apply_patch_args;
pub use standalone_executable::main;
pub use standalone_executable::run_apply_patch;

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
}

impl AppliedPatchDelta {
    fn new(changes: Vec<AppliedPatchChange>, exact: bool) -> Self {
        Self { changes, exact }
    }

    fn empty() -> Self {
        Self::new(Vec::new(), /*exact*/ true)
    }

    pub fn changes(&self) -> &[AppliedPatchChange] {
        &self.changes
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
/// A cancellation failure includes the complete delta of all committed hunks.
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
    let mut delta = AppliedPatchDelta::empty();
    match apply_hunks_to_files(hunks, cwd, fs, sandbox, &mut delta, is_cancelled).await {
        Ok(()) => {
            if let Err(error) = print_summary(hunks, stdout) {
                let _ = write!(stderr, "{error}\n{}", delta.failure_summary());
                return Err(ApplyPatchFailure::new(ApplyPatchError::from(error), delta));
            }
            Ok(delta)
        }
        Err(error) => {
            let msg = error.to_string();
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

/// Apply the hunks to the filesystem, recording committed changes in the delta.
/// Returns an error if the patch could not be applied.
async fn apply_hunks_to_files(
    hunks: &[Hunk],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    delta: &mut AppliedPatchDelta,
    is_cancelled: &(dyn Fn() -> bool + Sync),
) -> anyhow::Result<()> {
    if hunks.is_empty() {
        anyhow::bail!("No files were modified.");
    }

    invocation::validate_mutation_endpoints(hunks, cwd, fs, sandbox).await?;
    let mut prepared = preflight_hunks(hunks, cwd, fs, sandbox).await?;

    // Verify the complete read set before the first write. The executor gate
    // excludes cooperative writers; per-file checks below also catch external
    // edits. I/O failures still report the exact committed delta.
    for (index, (hunk, update)) in hunks.iter().zip(&mut prepared).enumerate() {
        if let Some(previous) = update {
            let path = hunk.resolve_path(cwd)?;
            if fs.read_file_text(&path, sandbox).await? != previous.original_contents {
                if let Hunk::UpdateFile { chunks, .. } = hunk {
                    *update = Some(
                        derive_new_contents_from_chunks(
                            &path,
                            chunks,
                            Some(index + 1),
                            fs,
                            sandbox,
                        )
                        .await?,
                    );
                } else {
                    anyhow::bail!(
                        "source changed during patch preparation: {}",
                        path.inferred_native_path_string()
                    );
                }
            }
        }
    }

    // A failed write can still have modified the target before surfacing an
    // error (for example by truncating before ENOSPC), so the accumulated
    // delta is no longer exact when a write fails.
    macro_rules! try_write {
        ($result:expr) => {
            match $result {
                Ok(value) => value,
                Err(error) => {
                    delta.exact = false;
                    return Err(anyhow::Error::from(error));
                }
            }
        };
    }

    // TODO(anp): Carry PathUri through committed patch deltas and the turn diff tracker.
    for (hunk_index, hunk) in hunks.iter().enumerate() {
        if is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Patch application cancelled before the next hunk",
            )
            .into());
        }
        let path_uri = hunk.resolve_path(cwd)?;
        match hunk {
            Hunk::AddFile { contents, .. } => {
                let overwritten_content =
                    read_optional_file_text_for_delta(&path_uri, fs, sandbox, &mut delta.exact)
                        .await;
                try_write!(
                    write_file_with_missing_parent_retry(
                        fs,
                        &path_uri,
                        contents.clone().into_bytes(),
                        sandbox,
                    )
                    .await
                );
                delta.changes.push(AppliedPatchChange {
                    path: path_uri.to_path_buf(),
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
                let deleted_content = fs.read_file_text(&path_uri, sandbox).await.ok();
                if deleted_content.is_none() {
                    delta.exact = false;
                }
                if let Err(error) = fs
                    .remove(
                        &path_uri,
                        RemoveOptions {
                            recursive: false,
                            force: false,
                        },
                        sandbox,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to delete file {}",
                            path_uri.inferred_native_path_string()
                        )
                    })
                {
                    delta.exact &= remove_failure_was_side_effect_free(
                        &path_uri,
                        deleted_content.as_deref(),
                        fs,
                        sandbox,
                    )
                    .await;
                    return Err(error);
                }
                if let Some(content) = deleted_content {
                    delta.changes.push(AppliedPatchChange {
                        path: path_uri.to_path_buf(),
                        change: AppliedPatchFileChange::Delete { content },
                    });
                }
            }
            Hunk::UpdateFile {
                move_path, chunks, ..
            } => {
                note_existing_path_delta_support(&path_uri, fs, sandbox, &mut delta.exact).await;
                if fs.read_file_text(&path_uri, sandbox).await?
                    != prepared[hunk_index]
                        .as_ref()
                        .expect("prepared update")
                        .original_contents
                {
                    prepared[hunk_index] = Some(
                        derive_new_contents_from_chunks(
                            &path_uri,
                            chunks,
                            Some(hunk_index + 1),
                            fs,
                            sandbox,
                        )
                        .await?,
                    );
                }
                let AppliedPatch {
                    original_contents,
                    new_contents,
                } = prepared[hunk_index]
                    .take()
                    .expect("updates are prepared before writes");
                anyhow::ensure!(
                    fs.read_file_text(&path_uri, sandbox).await? == original_contents,
                    "source changed after patch preparation: {}",
                    path_uri.inferred_native_path_string()
                );
                if let Some(dest) = move_path {
                    let dest_uri = cwd.join(&dest.to_string_lossy())?;
                    if invocation::mutation_endpoint_identity(fs, &path_uri, sandbox).await?
                        == invocation::mutation_endpoint_identity(fs, &dest_uri, sandbox).await?
                    {
                        return Err(ApplyPatchError::ParseError(InvalidPatchError(format!(
                            "move source and destination identify the same path: {}",
                            path_uri.inferred_native_path_string()
                        )))
                        .into());
                    }
                    let overwritten_move_content =
                        read_optional_file_text_for_delta(&dest_uri, fs, sandbox, &mut delta.exact)
                            .await;
                    try_write!(
                        write_file_with_missing_parent_retry(
                            fs,
                            &dest_uri,
                            new_contents.clone().into_bytes(),
                            sandbox,
                        )
                        .await
                    );
                    let dest_write_change_index = delta.changes.len();
                    delta.changes.push(AppliedPatchChange {
                        path: dest_uri.to_path_buf(),
                        change: AppliedPatchFileChange::Add {
                            content: new_contents.clone(),
                            overwritten_content: overwritten_move_content.clone(),
                        },
                    });
                    ensure_not_directory(&path_uri, fs, sandbox)
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to remove original {}",
                                path_uri.inferred_native_path_string()
                            )
                        })?;
                    if let Err(error) = fs
                        .remove(
                            &path_uri,
                            RemoveOptions {
                                recursive: false,
                                force: false,
                            },
                            sandbox,
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to remove original {}",
                                path_uri.inferred_native_path_string()
                            )
                        })
                    {
                        delta.exact &= remove_failure_was_side_effect_free(
                            &path_uri,
                            Some(&original_contents),
                            fs,
                            sandbox,
                        )
                        .await;
                        return Err(error);
                    }
                    delta.changes[dest_write_change_index] = AppliedPatchChange {
                        path: path_uri.to_path_buf(),
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
                        fs.write_file(&path_uri, new_contents.clone().into_bytes(), sandbox)
                            .await
                            .with_context(|| format!(
                                "Failed to write file {}",
                                path_uri.inferred_native_path_string()
                            ))
                    );
                    delta.changes.push(AppliedPatchChange {
                        path: path_uri.to_path_buf(),
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
                match derive_new_contents_from_chunks(&path, chunks, Some(ordinal + 1), fs, sandbox)
                    .await
                {
                    Ok(update) => prepared.push(Some(update)),
                    Err(error) => {
                        // Keep the ordered matching error, then enumerate other
                        // independently broken chunks without repeating its text.
                        let first = error.to_string();
                        failures.push(error);
                        for (index, chunk) in chunks.iter().enumerate() {
                            if let Err(mut error) = derive_new_contents_from_chunks(
                                &path,
                                std::slice::from_ref(chunk),
                                Some(ordinal + 1),
                                fs,
                                sandbox,
                            )
                            .await
                            {
                                if let ApplyPatchError::PatchContextMismatch(ref mut mismatch) =
                                    error
                                {
                                    mismatch.chunk_ordinal = index + 1;
                                }
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
            Hunk::AddFile { .. } => prepared.push(None),
        }
    }
    match failures.len() {
        0 => Ok(prepared),
        1 => Err(failures.remove(0)),
        _ => Err(ApplyPatchError::ComputeReplacements(format!(
            "Patch preflight found {} conflicts; no files were changed.\n{}",
            failures.len(),
            failures
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n\n")
        ))),
    }
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
        None => false,
    }
}

async fn read_optional_file_text_for_delta(
    path: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    exact: &mut bool,
) -> Option<String> {
    note_existing_path_delta_support(path, fs, sandbox, exact).await;
    match fs.read_file_text(path, sandbox).await {
        Ok(content) => Some(content),
        Err(source) if source.kind() == io::ErrorKind::NotFound => None,
        Err(_) => {
            *exact = false;
            None
        }
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
    contents: Vec<u8>,
    sandbox: Option<&FileSystemSandboxContext>,
) -> anyhow::Result<()> {
    match fs.write_file(path, contents.clone(), sandbox).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
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
            fs.write_file(path, contents, sandbox)
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

struct AppliedPatch {
    original_contents: String,
    new_contents: String,
}

/// Return *only* the new file contents (joined into a single `String`) after
/// applying the chunks to the file at `path`.
async fn derive_new_contents_from_chunks(
    path: &PathUri,
    chunks: &[UpdateFileChunk],
    hunk_ordinal: Option<usize>,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<AppliedPatch, ApplyPatchError> {
    let original_contents = fs.read_file_text(path, sandbox).await.map_err(|err| {
        ApplyPatchError::IoError(IoError {
            context: format!(
                "Failed to read file to update {}",
                path.inferred_native_path_string()
            ),
            source: err,
        })
    })?;

    if chunks.is_empty() {
        let new_contents = original_contents.clone();
        return Ok(AppliedPatch {
            original_contents,
            new_contents,
        });
    }

    // Match logical lines, but keep the original physical lines for untouched
    // regions so updates do not normalize their line endings.
    let physical_lines: Vec<String> = original_contents
        .split_inclusive('\n')
        .map(String::from)
        .collect();
    let original_lines: Vec<String> = physical_lines
        .iter()
        .map(|line| {
            line.strip_suffix("\r\n")
                .or_else(|| line.strip_suffix('\n'))
                .unwrap_or(line)
                .to_string()
        })
        .collect();
    let newline = if physical_lines
        .iter()
        .find(|line| line.ends_with('\n'))
        .is_some_and(|line| line.ends_with("\r\n"))
    {
        "\r\n"
    } else {
        "\n"
    };

    let path_text = path.inferred_native_path_string();
    let mut replacements = compute_replacements(
        &original_lines,
        &original_contents,
        path,
        &path_text,
        hunk_ordinal,
        chunks,
    )?;
    for (_, _, lines) in &mut replacements {
        for line in lines {
            line.push_str(newline);
        }
    }
    let mut new_lines = apply_replacements(physical_lines, &replacements);
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
    let new_contents = new_lines.concat();
    Ok(AppliedPatch {
        original_contents,
        new_contents,
    })
}

/// Compute a list of replacements needed to transform `original_lines` into the
/// new lines, given the patch `chunks`. Each replacement is returned as
/// `(start_index, old_len, new_lines)`.
fn compute_replacements(
    original_lines: &[String],
    original_contents: &str,
    path_uri: &PathUri,
    path: &str,
    hunk_ordinal: Option<usize>,
    chunks: &[UpdateFileChunk],
) -> std::result::Result<Vec<(usize, usize, Vec<String>)>, ApplyPatchError> {
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index: usize = 0;
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        if let Some(handle) = chunk
            .change_context
            .as_deref()
            .and_then(|s| s.strip_prefix("codex-range "))
        {
            let invalid = || {
                ApplyPatchError::ComputeReplacements(
                    "Invalid or stale codex-range handle; read current source before editing"
                        .into(),
                )
            };
            let (range, hash) = handle.split_once(" sha256:").ok_or_else(invalid)?;
            let (start, end) = range.split_once(':').ok_or_else(invalid)?;
            let start = start.parse::<usize>().map_err(|_| invalid())?;
            let end = end.parse::<usize>().map_err(|_| invalid())?;
            if start == 0
                || end < start
                || end > original_lines.len()
                || start - 1 < line_index
                || hash != format!("{:x}", Sha256::digest(original_contents.as_bytes()))
            {
                return Err(invalid());
            }
            if !chunk.old_lines.is_empty() && chunk.old_lines != original_lines[start - 1..end] {
                return Err(invalid());
            }
            replacements.push((start - 1, end - start + 1, chunk.new_lines.clone()));
            line_index = end;
            continue;
        }
        let source = || PatchMismatchSource {
            original_lines,
            original_contents,
            path: path_uri,
            hunk_ordinal,
        };
        let ambiguous_match = |error: seek_sequence::AmbiguousMatch| {
            located_patch_mismatch(
                source(),
                chunk_index + 1,
                PatchContextMismatchKind::AmbiguousMatch,
                format!("{error} in {path}; the excerpt shows the first candidate"),
                bounded_source_excerpt(original_lines, error.first_line - 1, error.first_line),
            )
        };
        // If a chunk has a `change_context`, we use seek_sequence to find it, then
        // adjust our `line_index` to continue from there.
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(idx) = seek_sequence::seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                /*eof*/ false,
            )
            .map_err(ambiguous_match)?
            {
                line_index = idx + 1;
            } else {
                let context = bounded_expected_lines(std::iter::once(ctx_line.as_str()));
                let message = format!("Failed to find context '{context}' in {path}");
                return Err(patch_context_mismatch(
                    PatchMismatchSource {
                        original_lines,
                        original_contents,
                        path: path_uri,
                        hunk_ordinal,
                    },
                    chunk_index + 1,
                    chunk,
                    PatchContextMismatchKind::ContextNotFound,
                    message,
                ));
            }
        }

        if chunk.old_lines.is_empty() {
            let insertion_idx = if chunk.change_context.is_some() {
                line_index
            } else {
                original_lines.len()
            };
            if chunk.is_end_of_file && insertion_idx != original_lines.len() {
                return Err(patch_context_mismatch(
                    source(),
                    chunk_index + 1,
                    chunk,
                    PatchContextMismatchKind::ExpectedLinesNotFound,
                    format!("End-of-file insertion anchor is not at the end of {path}"),
                ));
            }
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        // Parsed blank lines are source preconditions, not newline sentinels.
        let pattern: &[String] = &chunk.old_lines;
        let found =
            seek_sequence::seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file)
                .map_err(ambiguous_match)?;
        let new_slice: &[String] = &chunk.new_lines;

        if let Some(start_idx) = found {
            for operation in
                similar::capture_diff_slices(similar::Algorithm::Myers, pattern, new_slice)
            {
                // Context identifies the location; preserve its original bytes even
                // when it matched only after whitespace or Unicode normalization.
                if matches!(operation, similar::DiffOp::Equal { .. }) {
                    continue;
                }
                let old_range = operation.old_range();
                for old_index in old_range.clone() {
                    let expected = &pattern[old_index];
                    let actual = &original_lines[start_idx + old_index];
                    if !expected.trim().is_empty()
                        && expected[..expected.len() - expected.trim_start().len()]
                            != actual[..actual.len() - actual.trim_start().len()]
                    {
                        let line = start_idx + old_index;
                        return Err(located_patch_mismatch(
                            source(),
                            chunk_index + 1,
                            PatchContextMismatchKind::IndentationMismatch,
                            format!(
                                "Indentation differs on removed line {} in {path}; use the exact indentation in the current source excerpt.",
                                line + 1
                            ),
                            bounded_source_excerpt(original_lines, line, line + 1),
                        ));
                    }
                }
                replacements.push((
                    start_idx + old_range.start,
                    old_range.len(),
                    new_slice[operation.new_range()].to_vec(),
                ));
            }
            line_index = start_idx + pattern.len();
        } else {
            let message = format!(
                "Failed to find expected lines in {} after line {}. Chunks must be in top-to-bottom file order; check ordering and current context:\n{}",
                path,
                line_index,
                bounded_expected_lines(chunk.old_lines.iter().map(String::as_str)),
            );
            return Err(patch_context_mismatch(
                PatchMismatchSource {
                    original_lines,
                    original_contents,
                    path: path_uri,
                    hunk_ordinal,
                },
                chunk_index + 1,
                chunk,
                PatchContextMismatchKind::ExpectedLinesNotFound,
                message,
            ));
        }
    }

    replacements.sort_by_key(|(index, _, _)| *index);

    Ok(replacements)
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
    let current_content_sha256 =
        format!("{:x}", Sha256::digest(source.original_contents.as_bytes()));
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
    })
}

fn bounded_patch_mismatch_excerpt(
    original_lines: &[String],
    chunk: &UpdateFileChunk,
) -> Option<(usize, usize, String)> {
    if original_lines.is_empty() {
        return None;
    }
    // The @@ anchor need not be adjacent to the old block. Only old-line
    // offsets describe contiguous source; blank lines still occupy an offset.
    let expected = if chunk.old_lines.is_empty() {
        std::slice::from_ref(chunk.change_context.as_ref()?)
    } else {
        chunk.old_lines.as_slice()
    };
    let mut comparisons = 64 * 1024usize;
    let mut examined_bytes = 4 * 1024 * 1024usize;
    let mut equal = |left: &str, right: &str| -> Option<bool> {
        comparisons = comparisons.checked_sub(1)?;
        examined_bytes = examined_bytes.checked_sub(left.len().max(right.len()))?;
        Some(left == right)
    };
    let mut candidate_scores = BTreeMap::<usize, usize>::new();
    for (expected_offset, expected_line) in expected.iter().enumerate() {
        if expected_line.is_empty() {
            continue;
        }
        for (current_index, current_line) in original_lines.iter().enumerate().skip(expected_offset)
        {
            if !equal(current_line, expected_line)? {
                continue;
            }
            let start = current_index - expected_offset;
            if candidate_scores.contains_key(&start) {
                continue;
            }
            let mut score = 0;
            for (offset, line) in expected.iter().enumerate() {
                if let Some(current) = original_lines.get(start + offset) {
                    score += usize::from(equal(current, line)?);
                }
            }
            candidate_scores.insert(start, score);
        }
    }
    let best_score = candidate_scores.values().copied().max()?;
    let mut best = candidate_scores
        .into_iter()
        .filter_map(|(start, score)| (score == best_score).then_some(start));
    let candidate_start = best.next()?;
    if best.next().is_some() {
        return None;
    }
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
    unified_diff_from_chunks_internal(path, chunks, context, None, fs, sandbox).await
}

async fn unified_diff_from_chunks_internal(
    path: &PathUri,
    chunks: &[UpdateFileChunk],
    context: usize,
    hunk_ordinal: Option<usize>,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<ApplyPatchFileUpdate, ApplyPatchError> {
    let AppliedPatch {
        original_contents,
        new_contents,
    } = derive_new_contents_from_chunks(path, chunks, hunk_ordinal, fs, sandbox).await?;
    let text_diff = TextDiff::from_lines(&original_contents, &new_contents);
    let unified_diff = text_diff.unified_diff().context_radius(context).to_string();
    Ok(ApplyPatchFileUpdate {
        unified_diff,
        original_content: original_contents,
        content: new_contents,
    })
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
    use super::*;
    use codex_exec_server::LOCAL_FS;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::string::ToString;
    use tempfile::tempdir;

    /// Helper to construct a patch with the given body.
    fn wrap_patch(body: &str) -> String {
        format!("*** Begin Patch\n{body}\n*** End Patch")
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
    }

    #[tokio::test]
    async fn revision_bound_ranges_disambiguate_and_reject_stale_source() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.rs");
        let original = "same\r\nsame\r\nlast";
        fs::write(&path, original).unwrap();
        let hash = format!("{:x}", Sha256::digest(original));
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
        assert!(
            apply_patch(
                &patch,
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None
            )
            .await
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "same\r\nchanged\r\nlast"
        );
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
                format!("{:x}", Sha256::digest(original.as_bytes()))
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
    async fn out_of_order_chunks_explain_ordering_without_writing() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let original = "first\nlast\n";
        fs::write(dir.path().join("a.txt"), original).unwrap();
        let failure = apply_patch(
            &wrap_patch("*** Update File: a.txt\n@@\n-last\n+LAST\n@@\n-first\n+FIRST"),
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap_err();
        assert!(failure.delta().is_empty());
        let message = failure.to_string();
        assert!(message.contains("top-to-bottom file order"), "{message}");
        assert!(message.contains("after line 2"), "{message}");
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            original
        );
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
            format!("{:x}", Sha256::digest(post_prefix_contents.as_bytes()))
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
            format!("{:x}", Sha256::digest(lines[0].as_bytes()))
        );
        assert!(mismatch.to_string().contains("excerpt unavailable"));
    }

    #[test]
    fn patch_context_mismatch_excerpt_is_utf8_and_byte_bounded() {
        let long_line = "é".repeat(PATCH_MISMATCH_MAX_BYTES);
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
        assert_eq!(start, 1);
        assert!(end <= PATCH_MISMATCH_MAX_LINES);
        assert!(excerpt.len() <= PATCH_MISMATCH_MAX_BYTES);
        assert!(std::str::from_utf8(excerpt.as_bytes()).is_ok());
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

    /// Ensure that patches authored with ASCII characters can update lines that
    /// contain typographic Unicode punctuation (e.g. EN DASH, NON-BREAKING
    /// HYPHEN). Historically `git apply` succeeds in such scenarios but our
    /// internal matcher failed requiring an exact byte-for-byte match.  The
    /// fuzzy-matching pass that normalises common punctuation should now bridge
    /// the gap.
    #[tokio::test]
    async fn test_update_line_with_unicode_dash() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("unicode.py");

        // Original line contains EN DASH (\u{2013}) and NON-BREAKING HYPHEN (\u{2011}).
        let original = "import asyncio  # local import \u{2013} avoids top\u{2011}level dep\n";
        std::fs::write(&path, original).unwrap();

        // Patch uses plain ASCII dash / hyphen.
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
-import asyncio  # local import - avoids top-level dep
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
    async fn unicode_space_matching_preserves_nested_content_and_uses_first_match() {
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
            result.unwrap();
            assert_eq!(
                contents,
                original.replacen("label = \"a\u{00a0}b\"", "label = \"updated\"", 1)
            );
            assert!(stderr.is_empty());
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
            assert_eq!(fs::read_to_string(&created).unwrap(), "created\n");
            assert_eq!(dir.path().join("later.txt").exists(), !cancel);
            assert_eq!(
                stderr.matches(&format!("A {}", created.display())).count(),
                1
            );
            assert_eq!(
                stderr.contains(&format!("A {}", dir.path().join("later.txt").display())),
                !cancel
            );
            assert!(stderr.contains("do not retry the whole patch"), "{stderr}");
            assert!(
                stderr.contains(if cancel { "cancelled" } else { "output closed" }),
                "{stderr}"
            );
        }
    }

    #[tokio::test]
    async fn test_unreadable_destinations_return_inexact_delta() {
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
            let delta = apply_patch(
                &patch,
                &cwd,
                &mut stdout,
                &mut stderr,
                LOCAL_FS.as_ref(),
                /*sandbox*/ None,
            )
            .await
            .unwrap();

            assert!(!delta.is_exact());
        }
    }
}
