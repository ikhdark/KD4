//! Safety-buffering status and retry UI for active turns.

use super::*;
use codex_app_server_protocol::ModelSafetyBufferingUpdatedNotification;

const SAFETY_BUFFERING_PROMPT_VIEW_ID: &str = "safety-buffering-prompt";
const SAFETY_BUFFERING_LEARN_MORE_URL: &str = "https://help.openai.com/en/articles/20001326";

const SAFETY_BUFFERING_HEADER: &str =
    "Our systems are thinking a bit more about this request before responding.";
const SAFETY_BUFFERING_MESSAGE_WITH_RETRY: &str = "Hang tight or retry with a faster model for a quicker response, though it may be less capable of handling complex requests.";

#[derive(Debug)]
struct ActiveSafetyBuffering {
    turn_id: String,
    last_prompt_retry_model: Option<String>,
}

#[derive(Debug, Default)]
pub(super) struct SafetyBufferingState {
    submitted_turn: Option<(String, AppCommand)>,
    active: Option<ActiveSafetyBuffering>,
    agent_message_started: bool,
}

impl ChatWidget {
    pub(crate) fn record_safety_buffering_turn(&mut self, turn_id: String, turn: &AppCommand) {
        self.safety_buffering.submitted_turn = Some((turn_id, turn.clone()));
    }

    pub(super) fn reset_safety_buffering_for_turn_start(&mut self) {
        self.bottom_pane
            .dismiss_view_by_id(SAFETY_BUFFERING_PROMPT_VIEW_ID);
        self.safety_buffering.active = None;
        self.safety_buffering.agent_message_started = false;
    }

    pub(crate) fn clear_safety_buffering(&mut self) {
        self.bottom_pane
            .dismiss_view_by_id(SAFETY_BUFFERING_PROMPT_VIEW_ID);
        self.safety_buffering = SafetyBufferingState::default();
    }

    pub(super) fn mark_safety_buffering_agent_message_started(&mut self) {
        self.safety_buffering.agent_message_started = true;
    }

    pub(super) fn safety_buffering_is_waiting(&self) -> bool {
        self.safety_buffering.active.is_some() && !self.safety_buffering.agent_message_started
    }

    pub(crate) fn can_retry_safety_buffered_turn(&self, turn_id: &str) -> bool {
        self.turn_lifecycle.agent_turn_running
            && !self.safety_buffering.agent_message_started
            && self
                .safety_buffering
                .active
                .as_ref()
                .is_some_and(|active| active.turn_id == turn_id)
    }

    pub(crate) fn prepare_safety_buffering_retry(&mut self) {
        let cancel_edit = std::mem::take(&mut self.cancel_edit);
        self.last_rendered_user_message_display = None;
        self.finalize_turn();
        self.cancel_edit = cancel_edit;
        self.input_queue.user_turn_pending_start = true;
    }

    pub(crate) fn fail_safety_buffering_retry(&mut self) {
        self.input_queue.user_turn_pending_start = false;
        self.clear_safety_buffering();
        let prompt = self.cancel_edit.prompt.take();
        self.clear_cancel_edit();
        if let Some(prompt) = prompt {
            self.restore_user_message_to_composer(prompt);
        }
    }

    pub(super) fn on_model_safety_buffering_updated(
        &mut self,
        notification: ModelSafetyBufferingUpdatedNotification,
        replay_kind: Option<ReplayKind>,
    ) {
        let ModelSafetyBufferingUpdatedNotification {
            turn_id,
            show_buffering_ui,
            faster_model,
            ..
        } = notification;
        if matches!(replay_kind, Some(ReplayKind::ResumeInitialMessages))
            || !self.turn_lifecycle.agent_turn_running
            || self.turn_lifecycle.last_turn_id.as_deref() != Some(turn_id.as_str())
        {
            return;
        }
        if !show_buffering_ui {
            if self
                .safety_buffering
                .active
                .as_ref()
                .is_some_and(|active| active.turn_id == turn_id)
            {
                self.bottom_pane
                    .dismiss_view_by_id(SAFETY_BUFFERING_PROMPT_VIEW_ID);
                self.safety_buffering.active = None;
                self.restore_reasoning_status_header();
            }
            return;
        }

        if self.safety_buffering.agent_message_started {
            return;
        }

        let has_retry_turn =
            self.safety_buffering
                .submitted_turn
                .as_ref()
                .is_some_and(|(submitted_turn_id, _)| {
                    replay_kind.is_none() && submitted_turn_id == &turn_id
                });
        let thread_id = self.thread_id;
        let retry_model = faster_model.filter(|_| has_retry_turn && thread_id.is_some());
        let can_offer_retry = retry_model.is_some();
        let previous_active = self
            .safety_buffering
            .active
            .as_ref()
            .filter(|active| active.turn_id == turn_id);
        let should_show_prompt =
            previous_active.is_none_or(|active| active.last_prompt_retry_model != retry_model);
        self.safety_buffering.active = Some(ActiveSafetyBuffering {
            turn_id: turn_id.clone(),
            last_prompt_retry_model: retry_model.clone(),
        });

        let status_details = if can_offer_retry {
            format!("{SAFETY_BUFFERING_HEADER} {SAFETY_BUFFERING_MESSAGE_WITH_RETRY}")
        } else {
            SAFETY_BUFFERING_HEADER.to_string()
        };
        self.bottom_pane.ensure_status_indicator();
        self.set_status(
            "Working".to_string(),
            Some(status_details),
            StatusDetailsCapitalization::Preserve,
            /*details_max_lines*/ 6,
        );

        if !should_show_prompt {
            return;
        }
        self.bottom_pane
            .dismiss_view_by_id(SAFETY_BUFFERING_PROMPT_VIEW_ID);

        let mut header = vec![Box::new(
            Paragraph::new(Line::from(SAFETY_BUFFERING_HEADER).bold()).wrap(Wrap { trim: false }),
        ) as Box<dyn Renderable>];
        if can_offer_retry {
            header.push(Box::new(
                Paragraph::new(Line::from(SAFETY_BUFFERING_MESSAGE_WITH_RETRY).dim())
                    .wrap(Wrap { trim: false }),
            ));
        }
        let header = ColumnRenderable::with(header);
        let mut items = Vec::new();
        if let (Some(faster_model), Some((_, turn)), Some(thread_id)) = (
            retry_model,
            self.safety_buffering.submitted_turn.as_ref(),
            thread_id,
        ) {
            let turn = turn.clone();
            items.push(SelectionItem {
                name: "Retry with a faster model".to_string(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::RetrySafetyBufferedTurn {
                        thread_id,
                        turn_id: turn_id.clone(),
                        model: faster_model.clone(),
                        turn: turn.clone(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        items.extend([
            SelectionItem {
                name: "Keep waiting".to_string(),
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Learn more".to_string(),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::OpenUrlInBrowser {
                        url: SAFETY_BUFFERING_LEARN_MORE_URL.to_string(),
                    });
                })],
                ..Default::default()
            },
        ]);
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(SAFETY_BUFFERING_PROMPT_VIEW_ID),
            header: Box::new(header),
            items,
            ..Default::default()
        });
    }
}
