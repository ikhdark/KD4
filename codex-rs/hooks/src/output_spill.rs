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

#[derive(Clone)]
pub(crate) struct HookOutputSpiller {
    output_dir: AbsolutePathBuf,
    owner_thread_id: Option<ThreadId>,
}

impl HookOutputSpiller {
    pub(crate) fn new() -> Self {
        Self::with_directory(
            AbsolutePathBuf::resolve_path_against_base(std::env::temp_dir(), "/")
                .join(HOOK_OUTPUTS_DIR),
        )
    }

    pub(crate) fn with_directory(output_dir: AbsolutePathBuf) -> Self {
        Self {
            output_dir,
            owner_thread_id: None,
        }
    }

    pub(crate) fn for_thread(output_dir: AbsolutePathBuf, thread_id: ThreadId) -> Self {
        Self {
            output_dir,
            owner_thread_id: Some(thread_id),
        }
    }

    /// Keeps each hook text within the model-visible per-fragment budget.
    ///
    /// The session owner supplies its durable artifact root and reclaims files
    /// only when the owning thread is no longer resumable. Age and global quotas
    /// cannot establish that a path already advertised to the model is obsolete.
    pub(crate) async fn maybe_spill_text(&self, thread_id: ThreadId, text: String) -> String {
        self.spill_text(thread_id, text).await
    }

    async fn spill_text(&self, thread_id: ThreadId, text: String) -> String {
        if approx_token_count(&text) <= HOOK_OUTPUT_TOKEN_LIMIT {
            return text;
        }

        let path = hook_output_path(&self.output_dir, self.owner_thread_id.unwrap_or(thread_id));
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

    pub(crate) async fn maybe_spill_texts(
        &self,
        thread_id: ThreadId,
        texts: Vec<String>,
    ) -> Vec<String> {
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

fn hook_output_path(output_dir: &AbsolutePathBuf, thread_id: ThreadId) -> AbsolutePathBuf {
    output_dir
        .join(thread_id.to_string())
        .join("hooks")
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
