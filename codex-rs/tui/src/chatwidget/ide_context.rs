//! Chat-widget wiring for the `/ide` command and IDE context prompt injection.

use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use std::path::PathBuf;
use tokio_util::task::AbortOnDropHandle;
use uuid::Uuid;

use super::ChatWidget;
use super::QueuedUserMessage;
use super::ShellEscapePolicy;
use super::UserMessage;
use super::UserMessageHistoryRecord;
use super::user_message_for_restore;
use crate::app_event::AppEvent;
use crate::ide_context::IdeContext;

#[cfg(test)]
mod tests;

struct IdeContextRequest {
    id: Uuid,
    thread_id: Option<ThreadId>,
    cwd: PathBuf,
    initial_enablement: bool,
    _task: AbortOnDropHandle<()>,
}

#[derive(Default)]
pub(super) struct IdeContextState {
    enabled: bool,
    prompt_fetch_warned: bool,
    status_request: Option<IdeContextRequest>,
    prompt_request: Option<IdeContextRequest>,
    prompt_result: Option<Result<IdeContext, String>>,
    // Explicit submissions occupy the queue prefix; Tab-queued follow-ups remain behind them.
    submitted_count: usize,
}

impl IdeContextState {
    pub(super) fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn prompt_pending(&self) -> bool {
        self.prompt_request.is_some()
    }

    pub(super) fn invalidate_requests(&mut self) {
        self.status_request = None;
        self.invalidate_prompt();
    }

    pub(super) fn invalidate_prompt(&mut self) {
        self.prompt_request = None;
        self.prompt_result = None;
        self.submitted_count = 0;
    }

    pub(super) fn on_queue_tail_removed(&mut self, queue_len: usize) {
        if queue_len > 0 && queue_len <= self.submitted_count {
            self.submitted_count -= 1;
            if self.submitted_count == 0 {
                self.invalidate_prompt();
            }
        }
    }

    fn enable(&mut self) {
        self.enabled = true;
        self.prompt_fetch_warned = false;
    }

    fn disable(&mut self) {
        self.enabled = false;
        self.prompt_fetch_warned = false;
    }

    fn mark_available(&mut self) {
        self.prompt_fetch_warned = false;
    }
}

impl ChatWidget {
    pub(super) fn handle_ide_command(&mut self) {
        self.handle_ide_command_args("");
    }

    pub(super) fn handle_ide_command_args(&mut self, args: &str) {
        self.handle_ide_command_args_with_fetch(args, |cwd| {
            crate::ide_context::fetch_ide_context(cwd).map_err(|err| err.user_facing_hint())
        });
    }

    pub(super) fn handle_ide_command_args_with_fetch(
        &mut self,
        args: &str,
        fetch: impl FnOnce(&std::path::Path) -> Result<IdeContext, String> + Send + 'static,
    ) {
        let args = args.to_ascii_lowercase();
        let args = if args.is_empty() {
            if self.ide_context.is_enabled() {
                "off"
            } else {
                "on"
            }
        } else {
            args.as_str()
        };
        match args {
            "on" => {
                self.ide_context.enable();
                self.start_ide_status_request(true, fetch);
            }
            "off" => {
                self.cancel_pending_ide_prompt();
                self.ide_context.invalidate_requests();
                self.ide_context.disable();
                self.sync_ide_context_status_indicator();
                self.add_info_message("IDE context is off.".to_string(), None);
            }
            "status" => self.start_ide_status_request(false, fetch),
            _ => self.add_error_message("Usage: /ide [on|off|status]".to_string()),
        }
    }

    /// Consume only the fresh result fetched for this submission, never a cached status result.
    pub(super) fn maybe_apply_ide_context(&mut self, items: &mut Vec<UserInput>) {
        let Some(result) = self.ide_context.prompt_result.take() else {
            return;
        };

        match result {
            Ok(context) => {
                self.ide_context.mark_available();
                self.sync_ide_context_status_indicator();
                crate::ide_context::apply_ide_context_to_user_input(&context, items);
            }
            Err(err) => {
                self.sync_ide_context_status_indicator();
                if !self.ide_context.prompt_fetch_warned {
                    self.ide_context.prompt_fetch_warned = true;
                    self.add_info_message(
                        "IDE context was skipped for this message.".to_string(),
                        Some(err),
                    );
                }
            }
        }
    }

    /// Keep the payload in the normal queue so snapshots and draft editing retain ownership.
    pub(super) fn defer_prompt_for_ide_context(
        &mut self,
        user_message: &UserMessage,
        history_record: &UserMessageHistoryRecord,
        shell_escape_policy: ShellEscapePolicy,
    ) -> bool {
        let pending = self.ide_context.prompt_pending();
        if !pending
            && (!self.ide_context.is_enabled()
                || self.ide_context.prompt_result.is_some()
                || (shell_escape_policy == ShellEscapePolicy::Allow
                    && user_message.text.starts_with('!')))
        {
            return false;
        }
        let queued = QueuedUserMessage {
            shell_escape_policy,
            ..QueuedUserMessage::from(user_message.clone())
        };
        if pending {
            let index = self.ide_context.submitted_count;
            self.input_queue.queued_user_messages.insert(index, queued);
            self.input_queue
                .queued_user_message_history_records
                .insert(index, history_record.clone());
        } else {
            self.input_queue.queued_user_messages.push_front(queued);
            self.input_queue
                .queued_user_message_history_records
                .push_front(history_record.clone());
            self.ide_context.prompt_request = Some(self.spawn_ide_request(false, |cwd| {
                crate::ide_context::fetch_ide_context(cwd).map_err(|err| err.prompt_skip_hint())
            }));
        }
        self.ide_context.submitted_count += 1;
        self.update_task_running_state();
        self.refresh_pending_input_preview();
        self.request_redraw();
        true
    }

    fn spawn_ide_request(
        &self,
        initial_enablement: bool,
        fetch: impl FnOnce(&std::path::Path) -> Result<IdeContext, String> + Send + 'static,
    ) -> IdeContextRequest {
        let id = Uuid::new_v4();
        let cwd = self.config.cwd.to_path_buf();
        let fetch_cwd = cwd.clone();
        let tx = self.app_event_tx.clone();
        let task = tokio::spawn(async move {
            // Dropping the owner aborts the waiter. A started blocking pipe operation cannot be
            // aborted, but the transport's five-second deadline bounds its remaining lifetime.
            let result = tokio::task::spawn_blocking(move || fetch(&fetch_cwd))
                .await
                .unwrap_or_else(|err| Err(format!("IDE context request failed: {err}")));
            tx.send(AppEvent::IdeContextCompleted { id, result });
        });
        IdeContextRequest {
            id,
            thread_id: self.thread_id(),
            cwd,
            initial_enablement,
            _task: AbortOnDropHandle::new(task),
        }
    }

    fn start_ide_status_request(
        &mut self,
        initial_enablement: bool,
        fetch: impl FnOnce(&std::path::Path) -> Result<IdeContext, String> + Send + 'static,
    ) {
        if !self.ide_context.is_enabled() {
            self.add_ide_context_status_message(false, Err(String::new()));
            return;
        }
        // Repeated /ide on/status commands share the in-flight probe.
        if self.ide_context.status_request.is_none() {
            self.ide_context.status_request =
                Some(self.spawn_ide_request(initial_enablement, fetch));
        }
        self.sync_ide_context_status_indicator();
    }

    pub(crate) fn on_ide_context_completed(
        &mut self,
        id: Uuid,
        result: Result<IdeContext, String>,
    ) {
        if self
            .ide_context
            .status_request
            .as_ref()
            .is_some_and(|request| request.id == id)
        {
            let request = self
                .ide_context
                .status_request
                .take()
                .expect("matched request");
            if request.thread_id == self.thread_id() && request.cwd == self.config.cwd.as_path() {
                self.add_ide_context_status_message(request.initial_enablement, result);
            }
            return;
        }
        if !self
            .ide_context
            .prompt_request
            .as_ref()
            .is_some_and(|request| request.id == id)
        {
            return;
        }
        let request = self
            .ide_context
            .prompt_request
            .as_ref()
            .expect("matched request");
        if request.thread_id != self.thread_id() || request.cwd != self.config.cwd.as_path() {
            self.cancel_pending_ide_prompt();
            return;
        }
        self.ide_context.prompt_request = None;
        self.ide_context.submitted_count = self.ide_context.submitted_count.saturating_sub(1);
        self.update_task_running_state();
        let Some(queued) = self.input_queue.queued_user_messages.pop_front() else {
            return;
        };
        let history_record = self
            .input_queue
            .queued_user_message_history_records
            .pop_front()
            .unwrap_or(UserMessageHistoryRecord::UserMessageText);
        let shell_escape_policy = queued.shell_escape_policy;
        let user_message = queued.into_user_message();
        // Validation or dispatch can fail after the user has started another draft. The ordinary
        // synchronous restore path replaces the composer, so retain its current owner here.
        let composer = self.bottom_pane.composer_draft_snapshot();
        let restore_message = user_message_for_restore(user_message.clone(), &history_record);
        self.ide_context.prompt_result = Some(result);
        let (accepted, _) = self.submit_user_message_with_history_and_shell_escape_policy(
            user_message,
            history_record,
            shell_escape_policy,
            false,
        );
        self.ide_context.prompt_result = None;
        if !accepted {
            self.ide_context.submitted_count = 0;
            self.restore_ide_prompt_to_composer(restore_message, composer);
        }
        // Explicit Enter submissions are still steers, not next-turn Tab-queued messages.
        // Start their fresh fetch serially; shell commands bypass fetching through the normal path.
        while self.ide_context.submitted_count > 0 && !self.ide_context.prompt_pending() {
            let Some(queued) = self.input_queue.queued_user_messages.pop_front() else {
                self.ide_context.submitted_count = 0;
                break;
            };
            self.ide_context.submitted_count -= 1;
            let history = self
                .input_queue
                .queued_user_message_history_records
                .pop_front()
                .unwrap_or(UserMessageHistoryRecord::UserMessageText);
            let policy = queued.shell_escape_policy;
            let message = queued.into_user_message();
            let composer = self.bottom_pane.composer_draft_snapshot();
            let restore = user_message_for_restore(message.clone(), &history);
            if !self
                .submit_user_message_with_history_and_shell_escape_policy(
                    message, history, policy, false,
                )
                .0
            {
                self.ide_context.submitted_count = 0;
                self.restore_ide_prompt_to_composer(restore, composer);
            }
        }
        self.update_task_running_state();
        self.refresh_pending_input_preview();
        self.request_redraw();
    }

    fn add_ide_context_status_message(
        &mut self,
        initial_enablement: bool,
        result: Result<IdeContext, String>,
    ) {
        if !self.ide_context.is_enabled() {
            self.sync_ide_context_status_indicator();
            self.add_info_message("IDE context is off.".to_string(), /*hint*/ None);
            return;
        }

        match result {
            Ok(context) => {
                self.ide_context.mark_available();
                self.sync_ide_context_status_indicator();
                if crate::ide_context::has_prompt_context(&context) {
                    self.add_info_message(
                        "IDE context is on.".to_string(),
                        Some(
                            "Future messages will include your current IDE selection and open tabs."
                                .to_string(),
                        ),
                    );
                } else {
                    self.add_info_message(
                        "IDE context is on.".to_string(),
                        Some("Connected to your IDE.".to_string()),
                    );
                }
            }
            Err(err) => {
                if initial_enablement {
                    self.cancel_pending_ide_prompt();
                    self.ide_context.disable();
                }
                self.sync_ide_context_status_indicator();
                self.add_info_message(
                    if initial_enablement {
                        "IDE context could not be enabled."
                    } else {
                        "IDE context is on, but currently unavailable."
                    }
                    .to_string(),
                    Some(err),
                );
            }
        }
    }

    pub(super) fn sync_ide_context_status_indicator(&mut self) {
        self.bottom_pane
            .set_ide_context_active(self.ide_context.is_enabled());
    }
}
