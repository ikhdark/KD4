use codex_extension_api::ExtensionData;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

impl Session {
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

        for contributor in self.services.extensions.thread_lifecycle_contributors() {
            contributor
                .on_thread_idle(codex_extension_api::ThreadIdleInput {
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                })
                .await;
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

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn lifecycle_deadline_preserves_success_and_reports_stalls_and_panics() {
        assert!(bounded_lifecycle(async {}).await.is_ok());
        assert!(bounded_lifecycle(std::future::pending()).await.is_err());
        assert!(
            bounded_lifecycle(async { panic!("injected callback panic") })
                .await
                .is_err()
        );
    }
}
