//! Exact bounded-span selection; preflight and conditional publication remain
//! with the existing patch owner, rather than a separate editing tool.
use crate::ApplyPatchError;
use crate::UpdateFileChunk;
use crate::UpdateLineKind;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;

pub(crate) fn is_anchor_chunk(chunk: &UpdateFileChunk) -> bool {
    chunk
        .change_context
        .as_deref()
        .is_some_and(|context| context == "codex-anchor" || context.starts_with("codex-anchor "))
}

pub(crate) fn replace(
    path: &PathUri,
    source: &str,
    chunks: &[UpdateFileChunk],
) -> Result<String, ApplyPatchError> {
    let invalid = |message: &str| {
        ApplyPatchError::ComputeReplacements(format!(
            "Invalid codex-anchor in {}: {message}",
            path.inferred_native_path_string()
        ))
    };
    let [chunk] = chunks else {
        return Err(invalid(
            "use exactly one codex-anchor chunk per file; do not mix with other chunks",
        ));
    };
    let hash = chunk
        .change_context
        .as_deref()
        .and_then(|context| context.strip_prefix("codex-anchor sha256:"))
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| invalid("expected @@ codex-anchor sha256:<whole-file source_sha256>"))?;
    let current = hex::encode(Sha256::digest(source.as_bytes()));
    if !hash.eq_ignore_ascii_case(&current) {
        return Err(ApplyPatchError::RevisionGuardMismatch(format!(
            "codex-anchor revision guard failed for {}: expected {hash}, current {current}. Re-read and reconcile before submitting a fresh patch.",
            path.inferred_native_path_string()
        )));
    }
    if !chunk.old_lines.is_empty()
        || chunk.new_lines.len() != 1
        || chunk.is_end_of_file
        || chunk
            .line_kinds
            .iter()
            .any(|kind| *kind != UpdateLineKind::Add)
    {
        return Err(invalid(
            "expected one + line containing a JSON object with before, after, and replacement strings",
        ));
    }
    let edit: AnchorReplacement = serde_json::from_str(&chunk.new_lines[0]).map_err(|_| {
        invalid("expected a JSON object with only before, after, and replacement strings")
    })?;
    if edit.before.is_empty() || edit.after.is_empty() {
        return Err(invalid("before and after anchors must be nonempty"));
    }
    let unique = |anchor: &str, name: &str| -> Result<usize, ApplyPatchError> {
        let start = source
            .find(anchor)
            .ok_or_else(|| invalid(&format!("{name} anchor is missing")))?;
        // Unlike non-overlapping match iteration, first/last comparison also
        // rejects overlapping duplicates (for example, "aa" in "aaa").
        if source.rfind(anchor) != Some(start) {
            return Err(invalid(&format!("{name} anchor is ambiguous")));
        }
        Ok(start)
    };
    let start = unique(&edit.before, "before")? + edit.before.len();
    let end = unique(&edit.after, "after")?;
    if start > end {
        return Err(invalid(
            "anchors are reversed or overlapping; before must end at or before after starts",
        ));
    }
    // Retain both anchors and every other byte outside the selected interior,
    // including BOM, mixed line endings and the final-newline state.
    let mut result = source.to_owned();
    result.replace_range(start..end, &edit.replacement);
    Ok(result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnchorReplacement {
    before: String,
    after: String,
    replacement: String,
}
