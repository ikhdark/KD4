//! Interactive tool request surfaces for `ChatWidget`.
//!
//! This module owns approval, permission, elicitation, and user-input prompts
//! that block on user decisions.

use super::*;

impl ChatWidget {
    pub(super) fn on_exec_approval_request(&mut self, _id: String, ev: ExecApprovalRequestEvent) {
        self.record_visible_turn_activity();
        let ev2 = ev.clone();
        self.defer_or_handle(
            |q| q.push_exec_approval(ev),
            |s| s.handle_exec_approval_now(ev2),
        );
    }

    pub(super) fn on_apply_patch_approval_request(
        &mut self,
        _id: String,
        ev: ApplyPatchApprovalRequestEvent,
    ) {
        self.record_visible_turn_activity();
        let ev2 = ev.clone();
        self.defer_or_handle(
            |q| q.push_apply_patch_approval(ev),
            |s| s.handle_apply_patch_approval_now(ev2),
        );
    }

    pub(super) fn on_elicitation_request(
        &mut self,
        request_id: AppServerRequestId,
        params: McpServerElicitationRequestParams,
    ) {
        self.record_visible_turn_activity();
        let request_id2 = request_id.clone();
        let params2 = params.clone();
        self.defer_or_handle(
            |q| q.push_elicitation(request_id, params),
            |s| s.handle_elicitation_request_now(request_id2, params2),
        );
    }

    pub(super) fn on_request_user_input(&mut self, ev: ToolRequestUserInputParams) {
        self.record_visible_turn_activity();
        let ev2 = ev.clone();
        self.defer_or_handle(
            |q| q.push_user_input(ev),
            |s| s.handle_request_user_input_now(ev2),
        );
    }

    pub(super) fn on_request_permissions(&mut self, ev: RequestPermissionsEvent) {
        self.record_visible_turn_activity();
        let ev2 = ev.clone();
        self.defer_or_handle(
            |q| q.push_request_permissions(ev),
            |s| s.handle_request_permissions_now(ev2),
        );
    }

    pub(crate) fn handle_exec_approval_now(&mut self, ev: ExecApprovalRequestEvent) {
        self.flush_answer_stream_with_separator();
        let command = shlex::try_join(ev.command.iter().map(String::as_str))
            .unwrap_or_else(|_| ev.command.join(" "));
        self.notify(Notification::ExecApprovalRequested { command });

        let available_decisions = ev.effective_available_decisions();
        let request = ApprovalRequest::Exec {
            thread_id: self.thread_id.unwrap_or_default(),
            thread_label: None,
            id: ev.effective_approval_id(),
            environment_id: ev.environment_id,
            command: ev.command,
            reason: ev.reason,
            available_decisions,
            network_approval_context: ev.network_approval_context,
            additional_permissions: ev.additional_permissions,
        };
        self.bottom_pane
            .push_approval_request(request, &self.config.features);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }

    pub(crate) fn handle_apply_patch_approval_now(&mut self, ev: ApplyPatchApprovalRequestEvent) {
        self.flush_answer_stream_with_separator();

        let request = ApprovalRequest::ApplyPatch {
            thread_id: self.thread_id.unwrap_or_default(),
            thread_label: None,
            id: ev.call_id,
            reason: ev.reason,
            changes: ev.changes.clone(),
            cwd: self.config.cwd.clone(),
        };
        self.bottom_pane
            .push_approval_request(request, &self.config.features);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
        self.notify(Notification::EditApprovalRequested {
            cwd: self.config.cwd.to_path_buf(),
            changes: ev.changes.keys().cloned().collect(),
        });
    }

    pub(crate) fn handle_elicitation_request_now(
        &mut self,
        request_id: AppServerRequestId,
        params: McpServerElicitationRequestParams,
    ) {
        self.flush_answer_stream_with_separator();

        self.notify(Notification::ElicitationRequested {
            server_name: params.server_name.clone(),
        });

        let thread_id = ThreadId::from_string(&params.thread_id)
            .unwrap_or_else(|_| self.thread_id.unwrap_or_default());
        if let Some(params) = crate::bottom_pane::AppLinkViewParams::from_url_app_server_request(
            thread_id,
            &params.server_name,
            request_id.clone(),
            &params.request,
        ) {
            self.open_app_link_view(params);
        } else if let Some(request) = McpServerElicitationFormRequest::from_app_server_request(
            thread_id,
            request_id.clone(),
            params.clone(),
        ) {
            self.bottom_pane
                .push_mcp_server_elicitation_request(request);
        } else {
            match params.request {
                McpServerElicitationRequest::Form { message, .. } => {
                    let request = ApprovalRequest::McpElicitation {
                        thread_id,
                        thread_label: None,
                        server_name: params.server_name,
                        request_id,
                        message,
                    };
                    self.bottom_pane
                        .push_approval_request(request, &self.config.features);
                }
                McpServerElicitationRequest::OpenAiForm { .. }
                | McpServerElicitationRequest::Url { .. } => {
                    self.app_event_tx.resolve_elicitation(
                        thread_id,
                        params.server_name,
                        request_id,
                        codex_app_server_protocol::McpServerElicitationAction::Decline,
                        /*content*/ None,
                        /*meta*/ None,
                    );
                }
            }
        }
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }

    pub(crate) fn push_approval_request(&mut self, request: ApprovalRequest) {
        self.bottom_pane
            .push_approval_request(request, &self.config.features);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }

    pub(crate) fn push_mcp_server_elicitation_request(
        &mut self,
        request: McpServerElicitationFormRequest,
    ) {
        self.bottom_pane
            .push_mcp_server_elicitation_request(request);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }

    pub(crate) fn handle_request_user_input_now(&mut self, ev: ToolRequestUserInputParams) {
        self.flush_answer_stream_with_separator();
        let question_count = ev.questions.len();
        let summary = Notification::user_input_request_summary(&ev.questions);
        let title = match (question_count, summary.as_deref()) {
            (1, Some(summary)) => summary.to_string(),
            (1, None) => "Question requested".to_string(),
            (count, _) => format!("{count} questions requested"),
        };
        self.notify(Notification::PlanModePrompt { title });
        self.bottom_pane.push_user_input_request(ev);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }

    pub(crate) fn handle_request_permissions_now(&mut self, ev: RequestPermissionsEvent) {
        self.flush_answer_stream_with_separator();
        let request = ApprovalRequest::Permissions {
            thread_id: self.thread_id.unwrap_or_default(),
            thread_label: None,
            call_id: ev.call_id,
            environment_id: ev.environment_id,
            reason: ev.reason,
            permissions: ev.permissions,
        };
        self.bottom_pane
            .push_approval_request(request, &self.config.features);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }
}
