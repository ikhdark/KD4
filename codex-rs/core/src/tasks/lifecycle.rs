use codex_extension_api::ExtensionData;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

impl Session {
    pub(crate) async fn settle_extension_terminal_intents(
        &self,
        turn_store: &ExtensionData,
    ) -> Result<Vec<String>, codex_protocol::error::CodexErr> {
        let mut feedback = Vec::new();
        let result = bounded_lifecycle(async {
            for contributor in self.services.extensions.turn_lifecycle_contributors() {
                if let Some(message) = contributor
                    .on_turn_settled(codex_extension_api::TurnStopInput {
                        session_store: &self.services.session_extension_data,
                        thread_store: &self.services.thread_extension_data,
                        turn_store,
                    })
                    .await
                {
                    feedback.push(message);
                }
            }
        })
        .await;
        result.map_err(|_| codex_protocol::error::CodexErr::Fatal(
            "Extension terminal reconciliation failed or exceeded its deadline; staged goals remain unaccepted.".into()))?;
        Ok(feedback)
    }

    pub(super) async fn emit_turn_start_lifecycle(
        &self,
        turn_context: &TurnContext,
        token_usage_at_turn_start: &TokenUsage,
    ) {
        let mut first_panic = None;
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            let result = bounded_lifecycle(async {
                contributor
                    .on_turn_start(codex_extension_api::TurnStartInput {
                        turn_id: turn_context.sub_id.as_str(),
                        collaboration_mode: &turn_context.collaboration_mode,
                        token_usage_at_turn_start,
                        session_store: &self.services.session_extension_data,
                        thread_store: &self.services.thread_extension_data,
                        turn_store: turn_context.extension_data.as_ref(),
                    })
                    .await;
            })
            .await;
            if let Err(payload) = result
                && first_panic.is_none()
            {
                first_panic = Some(payload);
            }
        }
        if let Some(payload) = first_panic {
            std::panic::resume_unwind(payload);
        }
    }

    pub(super) async fn emit_turn_stop_lifecycle(&self, turn_store: &ExtensionData) {
        let mut first_panic = None;
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            let result = bounded_lifecycle(async {
                contributor
                    .on_turn_stop(codex_extension_api::TurnStopInput {
                        session_store: &self.services.session_extension_data,
                        thread_store: &self.services.thread_extension_data,
                        turn_store,
                    })
                    .await;
            })
            .await;
            if let Err(payload) = result
                && first_panic.is_none()
            {
                first_panic = Some(payload);
            }
        }
        if let Some(payload) = first_panic {
            std::panic::resume_unwind(payload);
        }
    }

    pub(crate) async fn emit_thread_idle_lifecycle_if_idle(&self) {
        if self.active_turn.lock().await.is_some()
            || self.input_queue.has_pending_turn_start_work().await
        {
            return;
        }

        // One budget for the whole phase, not one renewed budget per contributor.
        if bounded_lifecycle(async {
            for contributor in self.services.extensions.thread_lifecycle_contributors() {
                contributor
                    .on_thread_idle(codex_extension_api::ThreadIdleInput {
                        session_store: &self.services.session_extension_data,
                        thread_store: &self.services.thread_extension_data,
                    })
                    .await;
            }
        })
        .await
        .is_err()
        {
            tracing::warn!("thread idle lifecycle panicked or exceeded its phase deadline");
        }
    }

    pub(crate) async fn emit_thread_stop_lifecycle(&self) {
        if bounded_lifecycle(async {
            for contributor in self.services.extensions.thread_lifecycle_contributors() {
                contributor
                    .on_thread_stop(codex_extension_api::ThreadStopInput {
                        session_store: &self.services.session_extension_data,
                        thread_store: &self.services.thread_extension_data,
                    })
                    .await;
            }
        })
        .await
        .is_err()
        {
            tracing::warn!("thread stop lifecycle panicked or exceeded its phase deadline");
        }
    }

    pub(super) async fn emit_turn_abort_lifecycle(
        &self,
        reason: TurnAbortReason,
        turn_store: &ExtensionData,
    ) {
        let mut first_panic = None;
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            let result = bounded_lifecycle(async {
                contributor
                    .on_turn_abort(codex_extension_api::TurnAbortInput {
                        reason: reason.clone(),
                        session_store: &self.services.session_extension_data,
                        thread_store: &self.services.thread_extension_data,
                        turn_store,
                    })
                    .await;
            })
            .await;
            if let Err(payload) = result
                && first_panic.is_none()
            {
                first_panic = Some(payload);
            }
        }
        if let Some(payload) = first_panic {
            std::panic::resume_unwind(payload);
        }
    }

    pub(crate) async fn emit_turn_error_lifecycle(
        &self,
        turn_context: &TurnContext,
        error: CodexErrorInfo,
    ) {
        let mut first_panic = None;
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            let result = bounded_lifecycle(async {
                contributor
                    .on_turn_error(codex_extension_api::TurnErrorInput {
                        turn_id: turn_context.sub_id.as_str(),
                        error: error.clone(),
                        session_store: &self.services.session_extension_data,
                        thread_store: &self.services.thread_extension_data,
                        turn_store: turn_context.extension_data.as_ref(),
                    })
                    .await;
            })
            .await;
            if let Err(payload) = result
                && first_panic.is_none()
            {
                first_panic = Some(payload);
            }
        }
        if let Some(payload) = first_panic {
            std::panic::resume_unwind(payload);
        }
    }
}

// Callbacks may be dropped at the deadline. Required start failures use the
// existing worker-failure path; terminal failures use terminal fail-safe cleanup.
async fn bounded_lifecycle(
    future: impl std::future::Future<Output = ()>,
) -> Result<(), Box<dyn std::any::Any + Send>> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        AssertUnwindSafe(future).catch_unwind(),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(Box::new(
            "turn lifecycle callback exceeded its 30 second deadline".to_string(),
        )),
    }
}

