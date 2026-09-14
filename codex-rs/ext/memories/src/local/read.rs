use std::io::BufRead;

use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

use crate::DEFAULT_READ_MAX_TOKENS;
use crate::backend::MemoriesBackendError;
use crate::backend::ReadMemoryRequest;
use crate::backend::ReadMemoryResponse;

use super::LocalMemoriesBackend;
use super::path::reject_symlink;

pub(super) async fn read(
    backend: &LocalMemoriesBackend,
    request: ReadMemoryRequest,
) -> Result<ReadMemoryResponse, MemoriesBackendError> {
    if request.line_offset == 0 {
        return Err(MemoriesBackendError::InvalidLineOffset);
    }
    if request.max_lines == Some(0) {
        return Err(MemoriesBackendError::InvalidMaxLines);
    }

    let path = backend
        .resolve_scoped_path(Some(request.path.as_str()))
        .await?;
    let Some(metadata) = LocalMemoriesBackend::metadata_or_none(&path).await? else {
        return Err(MemoriesBackendError::NotFound { path: request.path });
    };
    reject_symlink(&request.path, &metadata)?;
    if !metadata.is_file() {
        return Err(MemoriesBackendError::NotFile { path: request.path });
    }

    let line_offset = request.line_offset;
    let max_lines = request.max_lines;
    let relative_path = request.path.clone();
    let (selected, has_suffix) = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                MemoriesBackendError::NotFound {
                    path: relative_path,
                }
            } else {
                err.into()
            }
        })?;
        read_range(std::io::BufReader::new(file), line_offset, max_lines)
    })
    .await
    .map_err(std::io::Error::other)??;
    let content_from_offset = selected.as_str();
    let max_tokens = if request.max_tokens == 0 {
        DEFAULT_READ_MAX_TOKENS
    } else {
        request.max_tokens
    };
    let content = truncate_text(content_from_offset, TruncationPolicy::Tokens(max_tokens));
    let truncated = has_suffix || content != content_from_offset;
    Ok(ReadMemoryResponse {
        path: request.path,
        start_line_number: request.line_offset,
        content,
        truncated,
    })
}

// Validate the entire UTF-8 file, as read_to_string did, but retain only the requested
// range. In particular, do not early-stop on a token budget: truncation keeps both ends.
fn read_range(
    mut reader: impl BufRead,
    line_offset: usize,
    max_lines: Option<usize>,
) -> Result<(String, bool), MemoriesBackendError> {
    let mut selected = String::new();
    let mut line = String::new();
    let mut current_line = 1usize;
    let mut has_suffix = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if current_line >= line_offset {
            if max_lines.is_none_or(|limit| current_line - line_offset < limit) {
                selected.push_str(&line);
            } else {
                has_suffix = true;
            }
        }
        // A final newline makes the empty next line addressable, matching the old offsets.
        if line.ends_with('\n') {
            current_line += 1;
        }
    }
    if current_line < line_offset {
        return Err(MemoriesBackendError::LineOffsetExceedsFileLength);
    }
    Ok((selected, has_suffix))
}
