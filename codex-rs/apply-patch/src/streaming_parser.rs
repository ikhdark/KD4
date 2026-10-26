use std::path::PathBuf;

use crate::parser::ADD_FILE_MARKER;
use crate::parser::BEGIN_PATCH_MARKER;
use crate::parser::CHANGE_CONTEXT_MARKER;
use crate::parser::DELETE_FILE_MARKER;
use crate::parser::EMPTY_CHANGE_CONTEXT_MARKER;
use crate::parser::END_PATCH_MARKER;
use crate::parser::EOF_MARKER;
use crate::parser::Hunk;
use crate::parser::MOVE_TO_MARKER;
use crate::parser::ParseError;
use crate::parser::UPDATE_FILE_MARKER;
use crate::parser::UpdateFileChunk;

use Hunk::*;
use ParseError::*;

const ENVIRONMENT_ID_MARKER: &str = "*** Environment ID:";

#[derive(Debug, Default, Clone)]
pub struct StreamingPatchParser {
    line_buffer: String,
    state: StreamingParserState,
    line_number: usize,
}

#[derive(Debug, Default, Clone)]
struct StreamingParserState {
    mode: StreamingParserMode,
    hunks: Vec<Hunk>,
    environment_id: Option<String>,
}

#[derive(Debug, Default, Clone, Copy)]
enum StreamingParserMode {
    #[default]
    NotStarted,
    StartedPatch,
    AddFile,
    DeleteFile,
    UpdateFile {
        hunk_line_number: usize,
    },
    EndedPatch,
}

impl StreamingPatchParser {
    pub fn environment_id(&self) -> Option<&str> {
        self.state.environment_id.as_deref()
    }

    pub fn hunks(&self) -> &[Hunk] {
        &self.state.hunks
    }

    fn ensure_update_hunk_is_not_empty(&self, line: &str) -> Result<(), ParseError> {
        if let Some(UpdateFile {
            path,
            move_path,
            chunks,
        }) = self.state.hunks.last()
        {
            if chunks.is_empty()
                && move_path.is_none()
                && let StreamingParserMode::UpdateFile { hunk_line_number } = self.state.mode
            {
                return Err(InvalidHunkError {
                    message: format!("Update file hunk for path '{}' is empty", path.display()),
                    line_number: hunk_line_number,
                });
            }
            if chunks
                .last()
                .is_some_and(|chunk| chunk.old_lines.is_empty() && chunk.new_lines.is_empty())
            {
                if line == END_PATCH_MARKER {
                    return Err(InvalidHunkError {
                        message: "Update hunk does not contain any lines".to_string(),
                        line_number: self.line_number,
                    });
                }
                return Err(InvalidHunkError {
                    message: format!(
                        "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                    ),
                    line_number: self.line_number,
                });
            }
        }
        Ok(())
    }

    fn handle_hunk_headers_and_end_patch(&mut self, trimmed: &str) -> Result<bool, ParseError> {
        if matches!(self.state.mode, StreamingParserMode::StartedPatch)
            && let Some(environment_id) = trimmed.strip_prefix(ENVIRONMENT_ID_MARKER)
        {
            if self.state.environment_id.is_some() {
                return Err(InvalidPatchError(
                    "apply_patch environment_id cannot be specified more than once".to_string(),
                ));
            }
            let environment_id = environment_id.trim();
            if environment_id.is_empty() {
                return Err(InvalidPatchError(
                    "apply_patch environment_id cannot be empty".to_string(),
                ));
            }
            self.state.environment_id = Some(environment_id.to_string());
            return Ok(true);
        }
        if trimmed == END_PATCH_MARKER {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.mode = StreamingParserMode::EndedPatch;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(ADD_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.hunks.push(AddFile {
                path: PathBuf::from(path),
                contents: String::new(),
            });
            self.state.mode = StreamingParserMode::AddFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(DELETE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.hunks.push(DeleteFile {
                path: PathBuf::from(path),
            });
            self.state.mode = StreamingParserMode::DeleteFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(UPDATE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.hunks.push(UpdateFile {
                path: PathBuf::from(path),
                move_path: None,
                chunks: Vec::new(),
            });
            self.state.mode = StreamingParserMode::UpdateFile {
                hunk_line_number: self.line_number,
            };
            return Ok(true);
        }
        Ok(false)
    }

    pub fn push_delta_in_place(&mut self, delta: &str) -> Result<(), ParseError> {
        for segment in delta.split_inclusive('\n') {
            if let Some(segment) = segment.strip_suffix('\n') {
                self.line_buffer.push_str(segment);
                let mut line = std::mem::take(&mut self.line_buffer);
                line.truncate(line.strip_suffix('\r').map_or(line.len(), str::len));
                self.line_number += 1;
                let result = self.process_line(&line);
                line.clear();
                self.line_buffer = line;
                result?;
            } else {
                self.line_buffer.push_str(segment);
            }
        }

        Ok(())
    }

    pub fn push_delta(&mut self, delta: &str) -> Result<Vec<Hunk>, ParseError> {
        self.push_delta_in_place(delta)?;
        Ok(self.hunks().to_vec())
    }

    pub fn finish_in_place(&mut self) -> Result<(), ParseError> {
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.line_number += 1;
            if line.trim() == END_PATCH_MARKER
                && !matches!(
                    self.state.mode,
                    StreamingParserMode::NotStarted | StreamingParserMode::EndedPatch
                )
            {
                self.ensure_update_hunk_is_not_empty(line.trim())?;
                self.state.mode = StreamingParserMode::EndedPatch;
            } else {
                self.process_line(&line)?;
            }
        }

        if !matches!(self.state.mode, StreamingParserMode::EndedPatch) {
            return Err(InvalidPatchError(
                "The last line of the patch must be '*** End Patch'".to_string(),
            ));
        }

        Ok(())
    }

    pub fn finish(&mut self) -> Result<Vec<Hunk>, ParseError> {
        self.finish_in_place()?;
        Ok(self.hunks().to_vec())
    }

    /// Finalize a batch parse without cloning the completed patch payload.
    pub(crate) fn into_parts(mut self) -> Result<(Vec<Hunk>, Option<String>), ParseError> {
        self.finish_in_place()?;
        Ok((self.state.hunks, self.state.environment_id))
    }

    fn process_line(&mut self, line: &str) -> Result<(), ParseError> {
        let trimmed = line.trim();
        match self.state.mode {
            StreamingParserMode::NotStarted => {
                if trimmed == BEGIN_PATCH_MARKER {
                    self.state.mode = StreamingParserMode::StartedPatch;
                    return Ok(());
                }
                Err(InvalidPatchError(
                    "The first line of the patch must be '*** Begin Patch'".to_string(),
                ))
            }
            StreamingParserMode::StartedPatch => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                Err(InvalidHunkError {
                    message: format!(
                        "'{trimmed}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
                    ),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::AddFile => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                if let Some(line_to_add) = line.strip_prefix('+')
                    && let Some(AddFile { contents, .. }) = self.state.hunks.last_mut()
                {
                    contents.push_str(line_to_add);
                    contents.push('\n');
                    return Ok(());
                }
                Err(InvalidHunkError {
                    message: format!(
                        "'{trimmed}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
                    ),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::DeleteFile => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                Err(InvalidHunkError {
                    message: format!(
                        "'{trimmed}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
                    ),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::UpdateFile { hunk_line_number } => {
                let update_line = line.trim_end();
                if self.handle_hunk_headers_and_end_patch(update_line)? {
                    return Ok(());
                }

                if let Some(UpdateFile {
                    move_path, chunks, ..
                }) = self.state.hunks.last_mut()
                {
                    if chunks.last().is_some_and(|chunk| chunk.is_end_of_file) {
                        if update_line.is_empty() {
                            return Ok(());
                        }
                        if update_line != EMPTY_CHANGE_CONTEXT_MARKER
                            && !update_line.starts_with(CHANGE_CONTEXT_MARKER)
                        {
                            return Err(InvalidHunkError {
                                message: format!(
                                    "Expected update hunk to start with a @@ context marker, got: '{line}'"
                                ),
                                line_number: self.line_number,
                            });
                        }
                    }

                    if chunks.is_empty()
                        && move_path.is_none()
                        && let Some(move_to_path) = update_line.strip_prefix(MOVE_TO_MARKER)
                    {
                        *move_path = Some(PathBuf::from(move_to_path));
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if (update_line == EMPTY_CHANGE_CONTEXT_MARKER
                        || update_line.starts_with(CHANGE_CONTEXT_MARKER))
                        && chunks.last().is_some_and(|chunk| {
                            chunk.old_lines.is_empty() && chunk.new_lines.is_empty()
                        })
                    {
                        return Err(InvalidHunkError {
                            message: format!(
                                "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                            ),
                            line_number: self.line_number,
                        });
                    }

                    if update_line == EMPTY_CHANGE_CONTEXT_MARKER {
                        chunks.push(UpdateFileChunk {
                            change_context: None,
                            old_lines: Vec::new(),
                            new_lines: Vec::new(),
                            is_end_of_file: false,
                            line_kinds: Vec::new(),
                        });
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(change_context) = update_line.strip_prefix(CHANGE_CONTEXT_MARKER) {
                        chunks.push(UpdateFileChunk {
                            change_context: Some(change_context.to_string()),
                            old_lines: Vec::new(),
                            new_lines: Vec::new(),
                            is_end_of_file: false,
                            line_kinds: Vec::new(),
                        });
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if update_line == EOF_MARKER {
                        if chunks.last().is_some_and(|chunk| {
                            chunk.old_lines.is_empty() && chunk.new_lines.is_empty()
                        }) {
                            return Err(InvalidHunkError {
                                message: "Update hunk does not contain any lines".to_string(),
                                line_number: self.line_number,
                            });
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.is_end_of_file = true;
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if line.is_empty() {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk {
                                change_context: None,
                                old_lines: Vec::new(),
                                new_lines: Vec::new(),
                                is_end_of_file: false,
                                line_kinds: Vec::new(),
                            });
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.line_kinds.push(crate::parser::UpdateLineKind::Context);
                            chunk.old_lines.push(String::new());
                            chunk.new_lines.push(String::new());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(line_to_add) = line.strip_prefix(' ') {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk {
                                change_context: None,
                                old_lines: Vec::new(),
                                new_lines: Vec::new(),
                                is_end_of_file: false,
                                line_kinds: Vec::new(),
                            });
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.line_kinds.push(crate::parser::UpdateLineKind::Context);
                            chunk.old_lines.push(line_to_add.to_string());
                            chunk.new_lines.push(line_to_add.to_string());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(line_to_add) = line.strip_prefix('+') {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk {
                                change_context: None,
                                old_lines: Vec::new(),
                                new_lines: Vec::new(),
                                is_end_of_file: false,
                                line_kinds: Vec::new(),
                            });
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.line_kinds.push(crate::parser::UpdateLineKind::Add);
                            chunk.new_lines.push(line_to_add.to_string());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(line_to_remove) = line.strip_prefix('-') {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk {
                                change_context: None,
                                old_lines: Vec::new(),
                                new_lines: Vec::new(),
                                is_end_of_file: false,
                                line_kinds: Vec::new(),
                            });
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.line_kinds.push(crate::parser::UpdateLineKind::Remove);
                            chunk.old_lines.push(line_to_remove.to_string());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if chunks.last().is_some_and(|chunk| {
                        !chunk.old_lines.is_empty() || !chunk.new_lines.is_empty()
                    }) {
                        return Err(InvalidHunkError {
                            message: format!(
                                "Expected update hunk to start with a @@ context marker, got: '{line}'"
                            ),
                            line_number: self.line_number,
                        });
                    }
                }
                Err(InvalidHunkError {
                    message: format!(
                        "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                    ),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::EndedPatch => {
                if trimmed.is_empty() {
                    Ok(())
                } else {
                    Err(InvalidPatchError(
                        "The last line of the patch must be '*** End Patch'".to_string(),
                    ))
                }
            }
        }
    }
}
