use crate::ApplyPatchError;
use crate::SourceHashRef;
use crate::UpdateFileChunk;
use crate::UpdateLineKind;
use codex_utils_path_uri::PathUri;
use sha2::Digest;
use sha2::Sha256;

pub(crate) const PREFIX: &str = "codex-replace ";

/// Derive an exact byte-preserving candidate; publication stays with the patch
/// owner, including whole-patch preflight, conditional writes and rollback.
pub(crate) fn replace(
    path: &PathUri,
    source: &str,
    chunks: &[UpdateFileChunk],
) -> Result<String, ApplyPatchError> {
    let invalid = |message: &str| {
        ApplyPatchError::ComputeReplacements(format!(
            "Invalid codex-replace in {}: {message}",
            path.inferred_native_path_string()
        ))
    };
    let current = hex::encode(Sha256::digest(source.as_bytes()));
    let mut replacements = Vec::new();
    let mut sites = Vec::new();
    for chunk in chunks {
        let descriptor = chunk
            .change_context
            .as_deref()
            .and_then(|context| context.strip_prefix(PREFIX))
            .ok_or_else(|| invalid("every chunk must be count-guarded; do not mix with line hunks"))?;
        let (count, hash) = descriptor
            .split_once(" sha256:")
            .ok_or_else(|| invalid(&format!("expected COUNT {}", SourceHashRef::FORM)))?;
        if count.is_empty() || !count.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid("expected a nonnegative occurrence count"));
        }
        let hash = SourceHashRef::parse(hash).map_err(|error| invalid(&error.to_string()))?;
        let expected = count
            .parse::<usize>()
            .map_err(|_| invalid("occurrence count is too large"))?;
        if !hash.matches(&current) {
            return Err(ApplyPatchError::RevisionGuardMismatch(format!(
                "codex-replace revision guard failed for {}: expected {hash}, current {current}. Re-read and reconcile before submitting a fresh patch.",
                path.inferred_native_path_string()
            )));
        }
        if chunk.is_end_of_file
            || chunk.old_lines.len() != 1
            || chunk.new_lines.len() != 1
            || (!chunk.line_kinds.is_empty()
                && chunk.line_kinds != [UpdateLineKind::Remove, UpdateLineKind::Add])
        {
            return Err(invalid(
                "expected one -JSON-string followed by one +JSON-string, without context or End of File markers",
            ));
        }
        let needle: String = serde_json::from_str(&chunk.old_lines[0])
            .map_err(|_| invalid("the removed line must contain one JSON string"))?;
        // Store replacement text once per chunk regardless of occurrence count.
        let replacement: String = serde_json::from_str(&chunk.new_lines[0])
            .map_err(|_| invalid("the added line must contain one JSON string"))?;
        if needle.is_empty() {
            return Err(invalid("the search string must not be empty"));
        }
        let matches = source.match_indices(needle.as_str());
        let observed = matches.clone().count();
        if observed != expected {
            return Err(ApplyPatchError::ComputeReplacements(format!(
                "codex-replace count mismatch for {}: expected {expected} exact non-overlapping occurrences, found {observed}",
                path.inferred_native_path_string()
            )));
        }
        if chunks.len() == 1 {
            return Ok(source.replace(needle.as_str(), &replacement));
        }
        let replacement_idx = replacements.len();
        replacements.push(replacement);
        sites.extend(matches.map(|(start, text)| (start, start + text.len(), replacement_idx)));
    }

    // Every chunk binds to the same original snapshot, including zero-count
    // assertions. Never search replacement bytes, nor allow ordered side effects.
    sites.sort_unstable_by_key(|&(start, _, _)| start);
    let mut cursor = 0;
    let mut result = String::with_capacity(source.len());
    for (start, end, replacement_idx) in sites {
        if start < cursor {
            return Err(invalid("replacement matches overlap across chunks"));
        }
        result.push_str(&source[cursor..start]);
        result.push_str(&replacements[replacement_idx]);
        cursor = end;
    }
    result.push_str(&source[cursor..]);
    Ok(result)
}
