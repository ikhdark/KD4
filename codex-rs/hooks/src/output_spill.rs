use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use tokio::sync::Mutex;

use codex_protocol::ThreadId;
use codex_protocol::items::HookPromptFragment;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::formatted_truncate_text;
use tokio::fs;
use tracing::warn;
use uuid::Uuid;

const HOOK_OUTPUTS_DIR: &str = "hook_outputs";
const HOOK_OUTPUT_TOKEN_LIMIT: usize = 2_500;
const HOOK_OUTPUT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const HOOK_OUTPUT_ACTIVE_GRACE: Duration = Duration::from_secs(60 * 60);
const HOOK_OUTPUT_MAX_FILES: usize = 512;
const HOOK_OUTPUT_MAX_BYTES: u64 = 64 * 1024 * 1024;
static LAST_GLOBAL_PRUNE: LazyLock<Arc<Mutex<Option<Instant>>>> = LazyLock::new(Arc::default);

#[derive(Clone, Copy)]
struct SpillRetentionPolicy {
    max_age: Duration,
    active_grace: Duration,
    max_files: usize,
    max_bytes: u64,
}

const SPILL_RETENTION_POLICY: SpillRetentionPolicy = SpillRetentionPolicy {
    max_age: HOOK_OUTPUT_MAX_AGE,
    active_grace: HOOK_OUTPUT_ACTIVE_GRACE,
    max_files: HOOK_OUTPUT_MAX_FILES,
    max_bytes: HOOK_OUTPUT_MAX_BYTES,
};

struct SpillFile {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

#[derive(Clone)]
pub(crate) struct HookOutputSpiller {
    output_dir: AbsolutePathBuf,
    last_prune: Arc<Mutex<Option<Instant>>>,
}

impl HookOutputSpiller {
    pub(crate) fn new() -> Self {
        Self {
            output_dir: AbsolutePathBuf::resolve_path_against_base(std::env::temp_dir(), "/")
                .join(HOOK_OUTPUTS_DIR),
            last_prune: Arc::clone(&LAST_GLOBAL_PRUNE),
        }
    }

    /// Keeps each hook text within the model-visible per-fragment budget.
    ///
    /// Oversized text is written in full under the OS temp directory at
    /// `<temp_dir>/hook_outputs/<thread_id>/`
    /// and replaced with the same head/tail preview style used for other truncated
    /// output, plus a path back to the preserved full text.
    pub(crate) async fn maybe_spill_text(&self, thread_id: ThreadId, text: String) -> String {
        if approx_token_count(&text) > HOOK_OUTPUT_TOKEN_LIMIT {
            self.prune_crash_leftovers(None).await;
        }
        self.spill_text(thread_id, text).await
    }

    async fn spill_text(&self, thread_id: ThreadId, text: String) -> String {
        if approx_token_count(&text) <= HOOK_OUTPUT_TOKEN_LIMIT {
            return text;
        }

        let path = hook_output_path(&self.output_dir, thread_id);
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent.as_ref()).await
        {
            warn!(
                "failed to create hook output directory {}: {err}",
                parent.display()
            );
            return formatted_truncate_text(
                &text,
                TruncationPolicy::Tokens(HOOK_OUTPUT_TOKEN_LIMIT),
            );
        }

        if let Err(err) = fs::write(path.as_ref(), &text).await {
            warn!("failed to write hook output {}: {err}", path.display());
            return formatted_truncate_text(
                &text,
                TruncationPolicy::Tokens(HOOK_OUTPUT_TOKEN_LIMIT),
            );
        }

        spilled_hook_output_preview(&text, &path)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "The try-lock prevents concurrent asynchronous pruning even when a prune exceeds the throttle interval"
    )]
    async fn prune_crash_leftovers(&self, protected_path: Option<&Path>) {
        let Ok(mut last_prune) = self.last_prune.try_lock() else {
            return;
        };
        if last_prune.is_some_and(|last| last.elapsed() < Duration::from_secs(60)) {
            return;
        }
        *last_prune = Some(Instant::now());
        if let Err(err) = prune_crash_leftovers_at(
            self.output_dir.as_ref(),
            protected_path,
            SPILL_RETENTION_POLICY,
            SystemTime::now(),
        )
        .await
        {
            warn!(
                "failed to prune hook output directory {}: {err}",
                self.output_dir.display()
            );
        }
    }

    pub(crate) async fn maybe_spill_texts(
        &self,
        thread_id: ThreadId,
        texts: Vec<String>,
    ) -> Vec<String> {
        if texts
            .iter()
            .any(|text| approx_token_count(text) > HOOK_OUTPUT_TOKEN_LIMIT)
        {
            self.prune_crash_leftovers(None).await;
        }
        let mut spilled = Vec::with_capacity(texts.len());
        for text in texts {
            spilled.push(self.spill_text(thread_id, text).await);
        }
        spilled
    }

    pub(crate) async fn maybe_spill_prompt_fragments(
        &self,
        thread_id: ThreadId,
        fragments: Vec<HookPromptFragment>,
    ) -> Vec<HookPromptFragment> {
        if fragments
            .iter()
            .any(|fragment| approx_token_count(&fragment.text) > HOOK_OUTPUT_TOKEN_LIMIT)
        {
            self.prune_crash_leftovers(None).await;
        }
        let mut spilled = Vec::with_capacity(fragments.len());
        for fragment in fragments {
            spilled.push(HookPromptFragment {
                text: self.spill_text(thread_id, fragment.text).await,
                hook_run_id: fragment.hook_run_id,
            });
        }
        spilled
    }
}

async fn prune_crash_leftovers_at(
    output_dir: &Path,
    protected_path: Option<&Path>,
    policy: SpillRetentionPolicy,
    now: SystemTime,
) -> std::io::Result<()> {
    let files = collect_spill_files(output_dir).await?;
    let mut retained = Vec::with_capacity(files.len());

    for file in files {
        let expired = age_at(now, file.modified) > policy.max_age;
        if expired && !is_protected(&file, protected_path) && remove_spill_file(&file).await {
            continue;
        }
        retained.push(file);
    }

    retained.sort_unstable_by_key(|file| file.modified);
    let mut retained_count = retained.len();
    let mut retained_bytes = retained.iter().map(|file| file.len).sum::<u64>();
    for file in &retained {
        if retained_count <= policy.max_files && retained_bytes <= policy.max_bytes {
            break;
        }
        if is_protected(file, protected_path) || age_at(now, file.modified) < policy.active_grace {
            continue;
        }
        if remove_spill_file(file).await {
            retained_count = retained_count.saturating_sub(1);
            retained_bytes = retained_bytes.saturating_sub(file.len);
        }
    }

    Ok(())
}

async fn collect_spill_files(output_dir: &Path) -> std::io::Result<Vec<SpillFile>> {
    let mut thread_dirs = match fs::read_dir(output_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut files = Vec::new();

    while let Some(thread_entry) = ignore_disappeared(thread_dirs.next_entry().await)?.flatten() {
        if !ignore_disappeared(thread_entry.file_type().await)?.is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        let thread_dir = thread_entry.path();
        let Some(mut thread_files) = ignore_disappeared(fs::read_dir(&thread_dir).await)? else {
            continue;
        };
        while let Some(file_entry) = ignore_disappeared(thread_files.next_entry().await)?.flatten()
        {
            if !ignore_disappeared(file_entry.file_type().await)?.is_some_and(|kind| kind.is_file())
                || file_entry
                    .path()
                    .extension()
                    .and_then(|value| value.to_str())
                    != Some("txt")
            {
                continue;
            }
            let Some(metadata) = ignore_disappeared(file_entry.metadata().await)? else {
                continue;
            };
            files.push(SpillFile {
                path: file_entry.path(),
                modified: metadata.modified()?,
                len: metadata.len(),
            });
        }
    }

    Ok(files)
}

fn ignore_disappeared<T>(result: std::io::Result<T>) -> std::io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn age_at(now: SystemTime, modified: SystemTime) -> Duration {
    now.duration_since(modified).unwrap_or_default()
}

fn is_protected(file: &SpillFile, protected_path: Option<&Path>) -> bool {
    protected_path.is_some_and(|protected_path| file.path == protected_path)
}

async fn remove_spill_file(file: &SpillFile) -> bool {
    match fs::remove_file(&file.path).await {
        // Keep directories stable while another writer is between mkdir and write.
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => {
            warn!(
                "failed to remove hook output {}: {err}",
                file.path.display()
            );
            false
        }
    }
}

fn hook_output_path(output_dir: &AbsolutePathBuf, thread_id: ThreadId) -> AbsolutePathBuf {
    output_dir
        .join(thread_id.to_string())
        .join(format!("{}.txt", Uuid::new_v4()))
}

/// Builds the model-visible replacement for a spilled hook output.
///
/// The path footer is budgeted before truncation so adding the recovery path
/// does not let the preview grow past the hook-output limit.
fn spilled_hook_output_preview(text: &str, path: &AbsolutePathBuf) -> String {
    let footer = format!("\n\nFull hook output saved to: {}", path.display());
    // A token policy keeps the formatter's warning and omission markers inside
    // its budget, so only the footer needs to be reserved.
    let preview_policy = TruncationPolicy::Tokens(
        HOOK_OUTPUT_TOKEN_LIMIT.saturating_sub(approx_token_count(&footer) + 1),
    );
    format!("{}{footer}", formatted_truncate_text(text, preview_policy))
}

#[cfg(test)]
#[path = "output_spill_tests.rs"]
mod tests;
