//! This module is responsible for parsing & validating a patch into a list of "hunks".
//! (It does not attempt to actually check that the patch can be applied to the filesystem.)
//!
//! The model-facing Lark rules live in `core/src/tools/handlers/apply_patch.lark`;
//! `apply_patch_spec.rs` supplies the start rule and optional environment header.
//! This parser also accepts compatibility forms, including leading/trailing
//! whitespace around patch markers and a shell heredoc wrapper.
use crate::ApplyPatchArgs;
use crate::streaming_parser::StreamingPatchParser;
use codex_utils_path_uri::PathUri;
use codex_utils_path_uri::PathUriParseError;
use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

pub(crate) const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
pub(crate) const END_PATCH_MARKER: &str = "*** End Patch";
pub(crate) const ADD_FILE_MARKER: &str = "*** Add File: ";
pub(crate) const DELETE_FILE_MARKER: &str = "*** Delete File: ";
pub(crate) const UPDATE_FILE_MARKER: &str = "*** Update File: ";
pub(crate) const MOVE_TO_MARKER: &str = "*** Move to: ";
pub(crate) const EOF_MARKER: &str = "*** End of File";
pub(crate) const CHANGE_CONTEXT_MARKER: &str = "@@ ";
pub(crate) const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";

/// Shared bound for stdin, argument, and in-process patch inputs.
pub(crate) const MAX_PATCH_INPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Error, Clone)]
pub enum ParseError {
    #[error("invalid patch: {0}")]
    InvalidPatchError(String),
    #[error("invalid hunk at line {line_number}, {message}")]
    InvalidHunkError { message: String, line_number: usize },
}
use ParseError::*;

#[derive(Debug, PartialEq, Clone)]
#[allow(clippy::enum_variant_names)]
pub enum Hunk {
    AddFile {
        path: PathBuf,
        contents: String,
    },
    DeleteFile {
        path: PathBuf,
    },
    UpdateFile {
        path: PathBuf,
        move_path: Option<PathBuf>,

        /// Chunks should be in order, i.e. the `change_context` of one chunk
        /// should occur later in the file than the previous chunk.
        chunks: Vec<UpdateFileChunk>,
    },
}

impl Hunk {
    pub fn resolve_path(&self, cwd: &PathUri) -> Result<PathUri, PathUriParseError> {
        let path = match self {
            Hunk::UpdateFile { path, .. } => path,
            Hunk::AddFile { .. } | Hunk::DeleteFile { .. } => self.path(),
        };
        cwd.join(&path.to_string_lossy())
    }

    /// Returns the path affected by this hunk, using the move destination for rename hunks.
    pub fn path(&self) -> &Path {
        match self {
            Hunk::AddFile { path, .. } => path,
            Hunk::DeleteFile { path } => path,
            Hunk::UpdateFile {
                move_path: Some(path),
                ..
            } => path,
            Hunk::UpdateFile {
                path,
                move_path: None,
                ..
            } => path,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpdateFileChunk {
    /// A source line immediately preceding the chunk, or a revision-bound
    /// `codex-range` descriptor. A textual anchor is not a containing scope.
    pub change_context: Option<String>,

    /// A contiguous block of lines that should be replaced with `new_lines`.
    /// With a textual `change_context`, `old_lines` must immediately follow it.
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    /// Original marker intent, retained because equal repeated lines can have
    /// different physical endings. Empty for legacy programmatic chunks.
    pub line_kinds: Vec<UpdateLineKind>,

    /// If set to true, `old_lines` must occur at the end of the source file.
    /// (Tolerance around trailing newlines should be encouraged.)
    pub is_end_of_file: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateLineKind { Context, Add, Remove }

// Preserve the existing logical-text equality contract. Marker provenance is
// used by application, not by callers comparing logical parser representations.
impl PartialEq for UpdateFileChunk {
    fn eq(&self, other: &Self) -> bool {
        self.change_context == other.change_context && self.old_lines == other.old_lines
            && self.new_lines == other.new_lines && self.is_end_of_file == other.is_end_of_file
    }
}

/// Parse patches leniently for compatibility with models that wrap patch text
/// in a shell heredoc. See [`ParseMode::Lenient`] for the accepted wrappers.
pub fn parse_patch(patch: &str) -> Result<ApplyPatchArgs, ParseError> {
    parse_patch_text(patch, ParseMode::Lenient)
}

enum ParseMode {
    /// GPT-4.1 is known to formulate the `command` array for the `local_shell`
    /// tool call for `apply_patch` call using something like the following:
    ///
    /// ```json
    /// [
    ///   "apply_patch",
    ///   "<<'EOF'\n*** Begin Patch\n*** Update File: README.md\n@@...\n*** End Patch\nEOF\n",
    /// ]
    /// ```
    ///
    /// This is a problem because `local_shell` is a bit of a misnomer: the
    /// `command` is not invoked by passing the arguments to a shell like Bash,
    /// but are invoked using something akin to `execvpe(3)`.
    ///
    /// This is significant in this case because where a shell would interpret
    /// `<<'EOF'...` as a heredoc and pass the contents via stdin (which is
    /// fine, as `apply_patch` is specified to read from stdin if no argument is
    /// passed), `execvpe(3)` interprets the heredoc as a literal string. To get
    /// the `local_shell` tool to run a command the way shell would, the
    /// `command` array must be something like:
    ///
    /// ```json
    /// [
    ///   "bash",
    ///   "-lc",
    ///   "apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: README.md\n@@...\n*** End Patch\nEOF\n",
    /// ]
    /// ```
    ///
    /// In lenient mode, we check if the argument to `apply_patch` starts with
    /// `<<'EOF'` and ends with `EOF\n`. If so, we strip off these markers,
    /// trim() the result, and treat what is left as the patch text.
    Lenient,
}

fn parse_patch_text(patch: &str, mode: ParseMode) -> Result<ApplyPatchArgs, ParseError> {
    if patch.len() > MAX_PATCH_INPUT_BYTES {
        return Err(InvalidPatchError(format!(
            "PATCH input exceeds the {MAX_PATCH_INPUT_BYTES}-byte limit"
        )));
    }
    // Keep CRLF bytes intact: the streaming parser strips exactly one CR.
    let patch = patch.trim();
    let boundaries = |source: &str| {
        let first = source.split('\n').next();
        let last = source.rsplit('\n').next();
        check_start_and_end_lines_strict(first.as_ref(), last.as_ref())
    };
    let patch = match boundaries(patch) {
        Ok(()) => patch,
        Err(original_error) => match mode {
            ParseMode::Lenient => {
                let first = patch.split('\n').next().unwrap_or_default();
                let last = patch.rsplit('\n').next().unwrap_or_default();
                if !matches!(first.strip_suffix('\r').unwrap_or(first), "<<EOF" | "<<'EOF'" | "<<\"EOF\"")
                    || last.trim() != "EOF"
                    || patch.split('\n').take(4).count() < 4
                {
                    return Err(original_error);
                }
                // Split/join previously reconstructed this same contiguous
                // byte span, including CR and extra-CR line endings.
                let (Some(start), Some(end)) = (patch.find('\n'), patch.rfind('\n')) else {
                    return Err(original_error);
                };
                let inner = &patch[start + 1..end];
                boundaries(inner)?;
                inner
            }
        },
    }.to_owned();
    let mut parser = StreamingPatchParser::default();
    parser.push_delta_in_place(&patch)?;
    let (hunks, environment_id) = parser.into_parts()?;
    Ok(ApplyPatchArgs {
        hunks,
        patch,
        workdir: None,
        environment_id,
    })
}

fn check_start_and_end_lines_strict(
    first_line: Option<&&str>,
    last_line: Option<&&str>,
) -> Result<(), ParseError> {
    let first_line = first_line.map(|line| line.trim());
    let last_line = last_line.map(|line| line.trim());

    match (first_line, last_line) {
        (Some(first), Some(last)) if first == BEGIN_PATCH_MARKER && last == END_PATCH_MARKER => {
            Ok(())
        }
        (Some(first), _) if first != BEGIN_PATCH_MARKER => Err(InvalidPatchError(String::from(
            "The first line of the patch must be '*** Begin Patch'",
        ))),
        _ => Err(InvalidPatchError(String::from(
            "The last line of the patch must be '*** End Patch'",
        ))),
    }
}
