use crate::MAX_LIST_RESULTS;
use crate::backend::ListMemoriesRequest;
use crate::backend::ListMemoriesResponse;
use crate::backend::MemoriesBackendError;
use crate::backend::MemoryEntry;
use crate::backend::MemoryEntryType;

use super::LocalMemoriesBackend;
use super::path::display_relative_path;
use super::path::is_hidden_path;
use super::path::read_sorted_dir_paths;
use super::path::reject_symlink;

pub(super) async fn list(
    backend: &LocalMemoriesBackend,
    request: ListMemoriesRequest,
) -> Result<ListMemoriesResponse, MemoriesBackendError> {
    let max_results = request.max_results.clamp(1, MAX_LIST_RESULTS);
    let start = backend.resolve_scoped_path(request.path.as_deref()).await?;
    let start_index = match request.cursor.as_deref() {
        Some(cursor) => cursor.parse::<usize>().map_err(|_| {
            MemoriesBackendError::invalid_cursor(cursor, "must be a non-negative integer")
        })?,
        None => 0,
    };
    let Some(metadata) = LocalMemoriesBackend::metadata_or_none(&start).await? else {
        return Err(MemoriesBackendError::NotFound {
            path: request.path.unwrap_or_default(),
        });
    };
    reject_symlink(&display_relative_path(&backend.root, &start), &metadata)?;

    let paths = if metadata.is_dir() {
        read_sorted_dir_paths(&start).await?
    } else if metadata.is_file() {
        vec![start.clone()]
    } else {
        Vec::new()
    };
    let mut entries = Vec::new();
    let mut seen = 0usize;
    let mut next_cursor = None;
    for path in paths {
        if is_hidden_path(&path) {
            continue;
        }
        let metadata = if path == start {
            metadata.clone()
        } else {
            let Some(metadata) = LocalMemoriesBackend::metadata_or_none(&path).await? else {
                continue;
            };
            metadata
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        let entry_type = if metadata.is_dir() {
            MemoryEntryType::Directory
        } else if metadata.is_file() {
            MemoryEntryType::File
        } else {
            continue;
        };
        if seen < start_index {
            seen += 1;
            continue;
        }
        if entries.len() == max_results {
            next_cursor = Some(seen.to_string());
            break;
        }
        entries.push(MemoryEntry {
            path: display_relative_path(&backend.root, &path),
            entry_type,
        });
        seen += 1;
    }
    if start_index > seen {
        return Err(MemoriesBackendError::invalid_cursor(
            start_index.to_string(),
            "exceeds result count",
        ));
    }
    let truncated = next_cursor.is_some();
    Ok(ListMemoriesResponse {
        path: request.path,
        entries,
        next_cursor,
        truncated,
    })
}
