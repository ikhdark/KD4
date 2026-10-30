//! Session-local, single-use retry receipts. Retain code, not authorization.
//! Every amended patch goes through ordinary verification, permissions and hooks.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::ApplyPatchArgs;
use codex_apply_patch::FileRevisionGuard;
use codex_apply_patch::Hunk;
use codex_apply_patch::UpdateFileChunk;
use codex_apply_patch::UpdateLineKind;
use codex_utils_path_uri::PathUri;
use serde_json::Value;

const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
const MAX_RETAINED_PATCHES: usize = 32;
const RETRY_HEADER: &str = "*** Retry Patch: ";

#[derive(Default)]
pub(crate) struct RetainedPatches {
    patches: BTreeMap<String, RetainedPatch>,
    bytes: usize,
    /// Why a receipt left `patches`, so a later submission can be told the
    /// actual cause instead of one message covering every possibility. Bounded
    /// to the same receipt count; ids are small and carry no patch payload.
    retired: BTreeMap<String, RetirementCause>,
}

#[derive(Clone, Copy)]
enum RetirementCause {
    /// Applied, or rotated by a hook, by an earlier call.
    Consumed,
    /// Dropped to stay inside the retention limits while newer receipts arrived.
    Evicted,
}

impl RetirementCause {
    fn describe(self) -> &'static str {
        match self {
            Self::Consumed => {
                "patch_id was already used by an earlier call; receipts are single-use. Inspect the current file before submitting a new patch."
            }
            Self::Evicted => {
                "patch_id was dropped to stay within the patch recovery limit after newer receipts were issued. Re-read the affected file and submit a fresh patch."
            }
        }
    }
}

struct RetainedPatch {
    patch: String,
    environment_id: String,
    environment_instance: String,
    cwd: PathUri,
    hook_recovery: Option<(String, Value)>,
}

pub(super) struct PreparedRetry {
    pub(super) id: String,
    pub(super) args: ApplyPatchArgs,
    pub(super) environment_id: String,
    pub(super) environment_instance: String,
    pub(super) cwd: PathUri,
}

enum Amendment {
    Hunk(usize, Hunk),
    Chunk(usize, usize, UpdateFileChunk),
}

/// A retry header opens the patch body, after an optional `*** Begin Patch`.
pub(super) fn is_retry(input: &str) -> bool {
    input
        .trim()
        .lines()
        .take(2)
        .any(|line| line.starts_with(RETRY_HEADER))
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
    // The opening line is optional: the retry header identifies the body. The
    // closing line is required, so a truncated retry is never applied.
    let header = usize::from(
        lines.first().map(|line| line.trim_end_matches('\r')) == Some("*** Begin Patch"),
    );
    if lines.len() < header + 2 || lines.last() != Some(&"*** End Patch") {
        return Err(
            "retry requires *** Retry Patch: <patch_id> and a final *** End Patch line".into(),
        );
    }
    let id = lines
        .get(header)
        .and_then(|line| line.strip_prefix(RETRY_HEADER))
        .map(|id| id.trim_end_matches('\r'))
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .ok_or("retry requires the returned patch_id")?
        .to_string();
    let mut amendments = Vec::new();
    let mut seen = BTreeSet::<(usize, Option<usize>)>::new();
    let mut i = header + 1;
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
        if !parsed.revision_guards.is_empty() {
            return Err("retry amendments cannot change revision guards; submit a fresh reconciled patch".into());
        }
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
    /// Rotate the receipt atomically: the old payload must never become
    /// replayable, even if execution is cancelled before workspace admission.
    pub(super) fn replace_after_hook(
        &mut self,
        id: &str,
        patch: &str,
        call_id: &str,
    ) -> Result<Value, String> {
        let saved = self
            .patches
            .remove(id)
            .ok_or_else(|| self.retirement_detail(id))?;
        self.bytes -= saved.patch.len();
        let receipt = self.retain(
            patch,
            &saved.environment_id,
            &saved.environment_instance,
            &saved.cwd,
            &Default::default(),
            None,
        );
        let Some(receipt) = receipt else {
            self.bytes += saved.patch.len();
            self.patches.insert(id.to_string(), saved);
            // Restored, so this id is live again and must not read as retired.
            self.retired.remove(id);
            return Err(
                "hook replacement cannot be retained within the patch recovery limit".into(),
            );
        };
        self.mark_retired(id, RetirementCause::Consumed);
        #[expect(
            clippy::expect_used,
            reason = "retain constructs patch_id as a string after inserting that exact key"
        )]
        let replacement_id = receipt["patch_id"].as_str().expect("retained patch id");
        #[expect(
            clippy::expect_used,
            reason = "retain just inserted this key and exclusive access prevents intervening removal"
        )]
        let replacement = self
            .patches
            .get_mut(replacement_id)
            .expect("new retained patch");
        replacement.hook_recovery = Some((call_id.to_string(), receipt.clone()));
        Ok(receipt)
    }

    pub(crate) fn recovery_for_call(&self, call_id: &str) -> Option<Value> {
        self.patches.values().find_map(|patch| {
            patch
                .hook_recovery
                .as_ref()
                .filter(|(owner, _)| owner == call_id)
                .map(|(_, receipt)| receipt.clone())
        })
    }

    pub(super) fn prepare(&self, input: &str) -> Result<PreparedRetry, String> {
        let (id, amendments) = parse_retry(input)?;
        let saved = self
            .patches
            .get(&id)
            .ok_or_else(|| self.retirement_detail(&id))?;
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
        args.patch = render_patch(&args.hunks, args.environment_id.as_deref(), &args.revision_guards);
        Ok(PreparedRetry {
            id,
            args,
            environment_id: saved.environment_id.clone(),
            environment_instance: saved.environment_instance.clone(),
            cwd: saved.cwd.clone(),
        })
    }

    /// Consume under the workspace gate, immediately before verification. Two
    /// concurrent calls cannot both execute the same retained mutation.
    pub(super) fn consume(&mut self, id: &str) -> Result<(), String> {
        let saved = self
            .patches
            .remove(id)
            .ok_or_else(|| self.retirement_detail(id))?;
        self.bytes -= saved.patch.len();
        self.mark_retired(id, RetirementCause::Consumed);
        Ok(())
    }

    /// Explain a receipt that is not live, naming the cause when it is known.
    /// An id this session never issued cannot be distinguished from one issued
    /// before a restart, because receipts do not survive one, nor from one
    /// retired long enough ago that the bounded cause record dropped it.
    fn retirement_detail(&self, id: &str) -> String {
        match self.retired.get(id) {
            Some(cause) => cause.describe().to_string(),
            None => {
                "patch_id is not a live receipt in this session: it was never issued here, was issued before a restart (receipts are session-local), or was retired too long ago for its cause to be retained. Inspect the current file and submit a new patch."
                    .to_string()
            }
        }
    }

    fn mark_retired(&mut self, id: &str, cause: RetirementCause) {
        // Keep the cause map bounded alongside the receipts themselves.
        while self.retired.len() >= MAX_RETAINED_PATCHES {
            let Some((oldest, _)) = self.retired.pop_first() else {
                break;
            };
            if oldest == id {
                break;
            }
        }
        self.retired.insert(id.to_string(), cause);
    }

    pub(super) fn retain(
        &mut self,
        patch: &str,
        environment_id: &str,
        environment_instance: &str,
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
        // A committed prefix may have changed a guarded revision. Never drop
        // its guard or silently replace the caller's baseline for a suffix.
        if count > 0 && !args.revision_guards.is_empty() {
            return None;
        }
        if count >= args.hunks.len() {
            return None;
        }
        let mut excluded = BTreeSet::new();
        for applied in committed.changes() {
            let mut candidates = args.hunks.iter().enumerate()
                .filter(|(_, hunk)| applied.matches_hunk(hunk, cwd));
            let (index, _) = candidates.next()?;
            // Ambiguous repeated operations and split moves remain withheld.
            if candidates.next().is_some() || !excluded.insert(index) { return None; }
        }
        let origins = (0..args.hunks.len()).filter(|index| !excluded.contains(index)).collect::<Vec<_>>();
        let remaining = origins.iter().map(|index| args.hunks[*index].clone()).collect::<Vec<_>>();
        let patch = render_patch(&remaining, args.environment_id.as_deref(), &args.revision_guards);
        if patch.len() > MAX_RETAINED_BYTES {
            return None;
        }
        while self.patches.len() >= MAX_RETAINED_PATCHES
            || self.bytes + patch.len() > MAX_RETAINED_BYTES
        {
            let (evicted_id, evicted) = self.patches.pop_first()?;
            self.bytes -= evicted.patch.len();
            self.mark_retired(&evicted_id, RetirementCause::Evicted);
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
                serde_json::json!({"hunk": index + 1, "original_hunk": origins[index] + 1, "path": hunk.path(), "chunks": chunks})
            })
            .collect::<Vec<_>>();
        self.bytes += patch.len();
        self.patches.insert(
            id.clone(),
            RetainedPatch {
                patch,
                environment_id: environment_id.into(),
                environment_instance: environment_instance.into(),
                cwd: cwd.clone(),
                hook_recovery: None,
            },
        );
        let instruction = format!(
            "Retry with this apply_patch input, keeping its first and last lines:\n*** Begin Patch\n*** Retry Patch: {id}\n*** Replace Chunk: <hunk> <chunk>\n<the corrected @@ chunk>\n*** End Patch\nTo replace a whole hunk, use *** Replace Hunk: <hunk> and its corrected file hunk. Omit unchanged contents. Indexes refer to remaining_hunks. The receipt is single-use and session-local; all source context and permissions are checked again."
        );
        Some(serde_json::json!({
            "patch_id": id, "committed_hunks_excluded": count, "remaining_hunks": hunks,
            "observed_source": observed_source,
            "instruction": instruction
        }))
    }
}

fn render_patch(hunks: &[Hunk], environment_id: Option<&str>, guards: &[FileRevisionGuard]) -> String {
    let mut text = String::from("*** Begin Patch\n");
    if let Some(id) = environment_id {
        text.push_str(&format!("*** Environment ID: {id}\n"));
    }
    for guard in guards {
        text.push_str(&format!("*** Guard File: {}\n*** Expect: ", guard.path.display()));
        match &guard.expected_sha256 {
            Some(hash) => text.push_str(&format!("sha256:{hash}\n")),
            None => text.push_str("absent\n"),
        }
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
                    if chunk.line_kinds.is_empty() {
                        // Legacy programmatic chunks have no marker provenance.
                        for line in &chunk.old_lines {
                            append_patch_line(&mut text, "-", line);
                        }
                        for line in &chunk.new_lines {
                            append_patch_line(&mut text, "+", line);
                        }
                    } else {
                        let (mut old, mut new) = (0, 0);
                        for kind in &chunk.line_kinds {
                            match kind {
                                UpdateLineKind::Context => {
                                    append_patch_line(&mut text, " ", &chunk.old_lines[old]);
                                    old += 1;
                                    new += 1;
                                }
                                UpdateLineKind::Remove => {
                                    append_patch_line(&mut text, "-", &chunk.old_lines[old]);
                                    old += 1;
                                }
                                UpdateLineKind::Add => {
                                    append_patch_line(&mut text, "+", &chunk.new_lines[new]);
                                    new += 1;
                                }
                            }
                        }
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


