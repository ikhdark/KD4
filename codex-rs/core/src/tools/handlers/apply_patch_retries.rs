//! Session-local, single-use retry receipts. Retain code, not authorization.
//! Every amended patch goes through ordinary verification, permissions and hooks.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::ApplyPatchArgs;
use codex_apply_patch::Hunk;
use codex_apply_patch::UpdateFileChunk;
use codex_utils_path_uri::PathUri;
use serde_json::Value;

const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
const MAX_RETAINED_PATCHES: usize = 32;
const RETRY_HEADER: &str = "*** Retry Patch: ";

#[derive(Default)]
pub(crate) struct RetainedPatches {
    patches: BTreeMap<String, RetainedPatch>,
    bytes: usize,
}

struct RetainedPatch {
    patch: String,
    environment_id: String,
    cwd: PathUri,
}

pub(super) struct PreparedRetry {
    pub(super) id: String,
    pub(super) args: ApplyPatchArgs,
    pub(super) environment_id: String,
    pub(super) cwd: PathUri,
}

enum Amendment {
    Hunk(usize, Hunk),
    Chunk(usize, usize, UpdateFileChunk),
}

pub(super) fn is_retry(input: &str) -> bool {
    input
        .trim()
        .lines()
        .nth(1)
        .is_some_and(|line| line.starts_with(RETRY_HEADER))
}

pub(super) fn validate_retry(input: &str) -> Result<(), String> {
    parse_retry(input).map(|_| ())
}

fn parse_retry(input: &str) -> Result<(String, Vec<Amendment>), String> {
    if input.len() > MAX_RETAINED_BYTES {
        return Err("retry amendments exceed the retained patch byte limit".into());
    }
    // Keep patch payload bytes: `.lines()` would strip literal trailing CRs
    // before the ordinary patch parser has a chance to decode them.
    let lines = input.trim().split('\n').collect::<Vec<_>>();
    if lines.first().map(|line| line.trim_end_matches('\r')) != Some("*** Begin Patch")
        || lines.last() != Some(&"*** End Patch")
    {
        return Err("retry requires *** Begin Patch and *** End Patch".into());
    }
    let id = lines
        .get(1)
        .and_then(|line| line.strip_prefix(RETRY_HEADER))
        .map(|id| id.trim_end_matches('\r'))
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .ok_or("retry requires the returned patch_id")?
        .to_string();
    let mut amendments = Vec::new();
    let mut seen = BTreeSet::<(usize, Option<usize>)>::new();
    let mut i = 2;
    while i < lines.len() - 1 {
        let (chunk, indexes) = if let Some(value) = lines[i].strip_prefix("*** Replace Chunk: ") {
            (true, value)
        } else if let Some(value) = lines[i].strip_prefix("*** Replace Hunk: ") {
            (false, value)
        } else {
            return Err(format!(
                "expected Replace Hunk or Replace Chunk at line {}",
                i + 1
            ));
        };
        let indexes = indexes
            .split_whitespace()
            .map(str::parse::<usize>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "retry indexes must be positive integers")?;
        if indexes.len() != if chunk { 2 } else { 1 } || indexes.contains(&0) {
            return Err(
                "Replace Hunk takes one 1-based index; Replace Chunk takes hunk and chunk indexes"
                    .into(),
            );
        }
        let hunk = indexes[0] - 1;
        let part = if chunk { Some(indexes[1] - 1) } else { None };
        if seen.iter().any(|&(other, piece)| {
            other == hunk && (piece == part || piece.is_none() || part.is_none())
        }) {
            return Err("retry amendments overlap; replace each hunk or chunk only once".into());
        }
        seen.insert((hunk, part));
        i += 1;
        let start = i;
        while i < lines.len() - 1 && !lines[i].starts_with("*** Replace ") {
            i += 1;
        }
        let body = lines[start..i].join("\n");
        let body = if chunk {
            format!("*** Update File: retained-chunk\n{body}")
        } else {
            body
        };
        let parsed =
            codex_apply_patch::parse_patch(&format!("*** Begin Patch\n{body}\n*** End Patch"))
                .map_err(|error| error.to_string())?;
        let mut hunks = parsed.hunks;
        if hunks.len() != 1 {
            return Err("each amendment must contain exactly one hunk".into());
        }
        let replacement = hunks.remove(0);
        amendments.push(if let Some(part) = part {
            let Hunk::UpdateFile {
                mut chunks,
                move_path: None,
                ..
            } = replacement
            else {
                return Err("Replace Chunk requires an update chunk".into());
            };
            if chunks.len() != 1 {
                return Err("Replace Chunk requires exactly one @@ chunk".into());
            }
            Amendment::Chunk(hunk, part, chunks.remove(0))
        } else {
            Amendment::Hunk(hunk, replacement)
        });
    }
    Ok((id, amendments))
}

impl RetainedPatches {
    pub(super) fn prepare(&self, input: &str) -> Result<PreparedRetry, String> {
        let (id, amendments) = parse_retry(input)?;
        let saved = self.patches.get(&id).ok_or("patch_id is unknown, expired or already used; inspect current files before submitting a new patch")?;
        let mut args = codex_apply_patch::parse_patch(&saved.patch).map_err(|e| e.to_string())?;
        for amendment in amendments {
            match amendment {
                Amendment::Hunk(index, hunk) => {
                    *args
                        .hunks
                        .get_mut(index)
                        .ok_or("retained hunk index is out of range")? = hunk;
                }
                Amendment::Chunk(index, part, replacement) => {
                    let Some(Hunk::UpdateFile { chunks, .. }) = args.hunks.get_mut(index) else {
                        return Err("retained hunk is not an update".into());
                    };
                    *chunks
                        .get_mut(part)
                        .ok_or("retained chunk index is out of range")? = replacement;
                }
            }
        }
        args.patch = render_patch(&args.hunks, args.environment_id.as_deref());
        Ok(PreparedRetry {
            id,
            args,
            environment_id: saved.environment_id.clone(),
            cwd: saved.cwd.clone(),
        })
    }

    /// Consume under the workspace gate, immediately before verification. Two
    /// concurrent calls cannot both execute the same retained mutation.
    pub(super) fn consume(&mut self, id: &str) -> Result<(), String> {
        let saved = self
            .patches
            .remove(id)
            .ok_or("patch_id was already consumed by another call")?;
        self.bytes -= saved.patch.len();
        Ok(())
    }

    pub(super) fn retain(
        &mut self,
        patch: &str,
        environment_id: &str,
        cwd: &PathUri,
        committed: &AppliedPatchDelta,
        observed_source: Option<Value>,
    ) -> Option<Value> {
        // Never derive a retry from uncertain writes, or guess which changes ran.
        if !committed.is_exact() {
            return None;
        }
        let args = codex_apply_patch::parse_patch(patch).ok()?;
        let count = committed.changes().len();
        if count >= args.hunks.len() {
            return None;
        }
        for (hunk, applied) in args.hunks.iter().zip(committed.changes()) {
            if hunk.resolve_path(cwd).ok()?.to_path_buf() != applied.path {
                return None;
            }
            let matches = matches!(
                (hunk, &applied.change),
                (
                    Hunk::AddFile { .. },
                    codex_apply_patch::AppliedPatchFileChange::Add { .. }
                ) | (
                    Hunk::DeleteFile { .. },
                    codex_apply_patch::AppliedPatchFileChange::Delete { .. }
                ) | (
                    Hunk::UpdateFile { .. },
                    codex_apply_patch::AppliedPatchFileChange::Update { .. }
                )
            );
            if !matches {
                return None;
            }
        }
        let remaining = &args.hunks[count..];
        let patch = render_patch(remaining, args.environment_id.as_deref());
        if patch.len() > MAX_RETAINED_BYTES {
            return None;
        }
        while self.patches.len() >= MAX_RETAINED_PATCHES
            || self.bytes + patch.len() > MAX_RETAINED_BYTES
        {
            let (_, evicted) = self.patches.pop_first()?;
            self.bytes -= evicted.patch.len();
        }
        let id = uuid::Uuid::now_v7().to_string();
        let hunks = remaining
            .iter()
            .enumerate()
            .map(|(index, hunk)| {
                let chunks = match hunk {
                    Hunk::UpdateFile { chunks, .. } => chunks.len(),
                    _ => 0,
                };
                serde_json::json!({"hunk": index + 1, "path": hunk.path(), "chunks": chunks})
            })
            .collect::<Vec<_>>();
        self.bytes += patch.len();
        self.patches.insert(
            id.clone(),
            RetainedPatch {
                patch,
                environment_id: environment_id.into(),
                cwd: cwd.clone(),
            },
        );
        Some(serde_json::json!({
            "patch_id": id, "committed_hunks_excluded": count, "remaining_hunks": hunks,
            "observed_source": observed_source,
            "instruction": "Use apply_patch with *** Retry Patch: <patch_id>, then *** Replace Chunk: <hunk> <chunk> and its corrected @@ chunk, or *** Replace Hunk: <hunk> and its corrected file hunk. Omit unchanged contents. Indexes refer to remaining_hunks. The receipt is single-use and session-local; all source context and permissions are checked again."
        }))
    }
}

fn render_patch(hunks: &[Hunk], environment_id: Option<&str>) -> String {
    let mut text = String::from("*** Begin Patch\n");
    if let Some(id) = environment_id {
        text.push_str(&format!("*** Environment ID: {id}\n"));
    }
    for hunk in hunks {
        match hunk {
            Hunk::AddFile { path, contents } => {
                text.push_str(&format!("*** Add File: {}\n", path.display()));
                for line in contents.split_terminator('\n') {
                    append_patch_line(&mut text, "+", line);
                }
            }
            Hunk::DeleteFile { path } => {
                text.push_str(&format!("*** Delete File: {}\n", path.display()))
            }
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                text.push_str(&format!("*** Update File: {}\n", path.display()));
                if let Some(path) = move_path {
                    text.push_str(&format!("*** Move to: {}\n", path.display()));
                }
                for chunk in chunks {
                    match &chunk.change_context {
                        Some(context) => append_patch_line(&mut text, "@@ ", context),
                        None => text.push_str("@@\n"),
                    }
                    for line in &chunk.old_lines {
                        append_patch_line(&mut text, "-", line);
                    }
                    for line in &chunk.new_lines {
                        append_patch_line(&mut text, "+", line);
                    }
                    if chunk.is_end_of_file {
                        text.push_str("*** End of File\n");
                    }
                }
            }
        }
    }
    text.push_str("*** End Patch");
    text
}

fn append_patch_line(text: &mut String, prefix: &str, line: &str) {
    text.push_str(prefix);
    text.push_str(line);
    if line.ends_with('\r') {
        text.push('\r');
    }
    text.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_exec_server::LOCAL_FS;

    fn retained(store: &mut RetainedPatches, cwd: &PathUri, patch: &str) -> String {
        store
            .retain(patch, "local", cwd, &Default::default(), None)
            .unwrap()["patch_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn retained_and_amended_code_preserve_literal_carriage_returns() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let patch = "*** Begin Patch\n*** Add File: a\n+literal\r\r\n*** Update File: b\n@@\n-old\r\r\n+new\r\r\n*** End Patch";
        let mut store = RetainedPatches::default();
        let id = retained(&mut store, &cwd, patch);
        let retry = format!("*** Begin Patch\n*** Retry Patch: {id}\n*** End Patch");
        assert_eq!(
            store.prepare(&retry).unwrap().args.hunks,
            codex_apply_patch::parse_patch(patch).unwrap().hunks
        );
        let retry = format!(
            "*** Begin Patch\n*** Retry Patch: {id}\n*** Replace Chunk: 2 1\n@@\n-current\r\r\n+fixed\r\r\n*** End Patch"
        );
        let prepared = store.prepare(&retry).unwrap();
        let Hunk::UpdateFile { chunks, .. } = &prepared.args.hunks[1] else {
            panic!("update")
        };
        assert_eq!(chunks[0].new_lines, ["fixed\r"]);
        assert_eq!(
            codex_apply_patch::parse_patch(&prepared.args.patch)
                .unwrap()
                .hunks,
            prepared.args.hunks
        );
    }

    #[test]
    fn amendments_preserve_unmentioned_chunks_and_are_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let patch = "*** Begin Patch\n*** Update File: a\n@@ first\n-old\n+new\n@@ second\n-two\n+three\n*** Add File: b\n+unchanged\n*** End Patch";
        let mut store = RetainedPatches::default();
        let id = retained(&mut store, &cwd, patch);
        let retry = format!(
            "*** Begin Patch\n*** Retry Patch: {id}\n*** Replace Chunk: 1 1\n@@ first\n-current\n+new\n*** End Patch"
        );
        let prepared = store.prepare(&retry).unwrap();
        let original = codex_apply_patch::parse_patch(patch).unwrap();
        assert_eq!(prepared.args.hunks[1], original.hunks[1]);
        let Hunk::UpdateFile { chunks, .. } = &prepared.args.hunks[0] else {
            panic!("update")
        };
        assert_eq!(chunks[0].old_lines, ["current"]);
        let Hunk::UpdateFile { chunks: old, .. } = &original.hunks[0] else {
            panic!("update")
        };
        assert_eq!(chunks[1], old[1]);
        assert_eq!(
            codex_apply_patch::parse_patch(&prepared.args.patch)
                .unwrap()
                .hunks,
            prepared.args.hunks
        );
        store.consume(&id).unwrap();
        assert!(store.consume(&id).is_err());
        assert!(store.prepare(&retry).is_err());
        assert_eq!(store.bytes, 0);
    }

    #[test]
    fn invalid_or_overlapping_amendments_do_not_consume_the_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let mut store = RetainedPatches::default();
        let id = retained(
            &mut store,
            &cwd,
            "*** Begin Patch\n*** Add File: a\n+x\n*** End Patch",
        );
        for edits in [
            "*** Replace Hunk: 0\n*** Add File: a\n+y\n",
            "*** Replace Hunk: 2\n*** Add File: a\n+y\n",
            "*** Replace Chunk: 1 1\n@@\n-x\n+y\n",
            "*** Replace Hunk: 1\n*** Add File: a\n+y\n*** Replace Hunk: 1\n*** Add File: a\n+z\n",
        ] {
            assert!(
                store
                    .prepare(&format!(
                        "*** Begin Patch\n*** Retry Patch: {id}\n{edits}*** End Patch"
                    ))
                    .is_err(),
                "{edits}"
            );
        }
        let retry = format!(
            "*** Begin Patch\n*** Retry Patch: {id}\n*** Replace Hunk: 1\n*** Add File: a\n+corrected\n*** End Patch"
        );
        assert!(
            store
                .prepare(&retry)
                .unwrap()
                .args
                .patch
                .contains("+corrected")
        );
        assert!(
            RetainedPatches::default().prepare(&retry).is_err(),
            "session-local IDs cannot resurrect writes after restart"
        );
    }

    #[tokio::test]
    async fn runtime_partial_commit_is_excluded_from_the_retained_patch() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let patch = "*** Begin Patch\n*** Add File: committed\n+first\n*** Update File: pending\n@@\n-old\n+new\n*** End Patch";
        std::fs::write(dir.path().join("pending"), "old\n").unwrap();
        // Preflight rejects stale sources before any write, so an exact partial
        // commit comes from cancellation between hunks.
        let committed = dir.path().join("committed");
        let failure = codex_apply_patch::apply_patch_with_cancellation(
            patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
            &|| committed.exists(),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.delta().changes().len(), 1);
        let mut store = RetainedPatches::default();
        let receipt = store
            .retain(patch, "local", &cwd, failure.delta(), None)
            .unwrap();
        assert_eq!(receipt["committed_hunks_excluded"], 1);
        std::fs::write(dir.path().join("committed"), "later independent edit\n").unwrap();
        let retry = store
            .prepare(&format!(
                "*** Begin Patch\n*** Retry Patch: {}\n*** End Patch",
                receipt["patch_id"].as_str().unwrap()
            ))
            .unwrap();
        assert_eq!(retry.args.hunks.len(), 1);
        codex_apply_patch::apply_patch(
            &retry.args.patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("committed")).unwrap(),
            "later independent edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("pending")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn retention_eviction_does_not_allow_replaying_expired_ids() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let patch = "*** Begin Patch\n*** Add File: a\n+x\n*** End Patch";
        let mut store = RetainedPatches::default();
        let first = retained(&mut store, &cwd, patch);
        for _ in 0..MAX_RETAINED_PATCHES {
            retained(&mut store, &cwd, patch);
        }
        assert!(
            store
                .prepare(&format!(
                    "*** Begin Patch\n*** Retry Patch: {first}\n*** End Patch"
                ))
                .is_err()
        );
        assert_eq!(store.patches.len(), MAX_RETAINED_PATCHES);
        assert!(store.bytes <= MAX_RETAINED_BYTES);
    }
}
