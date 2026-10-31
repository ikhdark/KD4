use crate::ApplyPatchError;
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
    if chunks.len() != 1 {
        return Err(invalid(
            "use exactly one count-guarded replacement chunk per file; do not mix with line hunks",
        ));
    }
    let chunk = &chunks[0];
    let descriptor = chunk
        .change_context
        .as_deref()
        .and_then(|context| context.strip_prefix(PREFIX))
        .ok_or_else(|| invalid("missing replacement descriptor"))?;
    let (count, hash) = descriptor
        .split_once(" sha256:")
        .ok_or_else(|| invalid("expected COUNT sha256:<64 hexadecimal characters>"))?;
    if count.is_empty()
        || !count.bytes().all(|byte| byte.is_ascii_digit())
        || hash.len() != 64
        || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid(
            "expected a nonnegative occurrence count and a whole-file SHA-256",
        ));
    }
    let expected = count
        .parse::<usize>()
        .map_err(|_| invalid("occurrence count is too large"))?;
    let current = hex::encode(Sha256::digest(source.as_bytes()));
    if !current.eq_ignore_ascii_case(hash) {
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
    let replacement: String = serde_json::from_str(&chunk.new_lines[0])
        .map_err(|_| invalid("the added line must contain one JSON string"))?;
    if needle.is_empty() {
        return Err(invalid("the search string must not be empty"));
    }
    let observed = source.match_indices(needle.as_str()).count();
    if observed != expected {
        return Err(ApplyPatchError::ComputeReplacements(format!(
            "codex-replace count mismatch for {}: expected {expected} exact non-overlapping occurrences, found {observed}",
            path.inferred_native_path_string()
        )));
    }
    // Literal replacement does not reinterpret $, backslashes, Unicode or line
    // endings. New text is never searched again; zero asserts absence.
    Ok(source.replace(needle.as_str(), &replacement))
}
