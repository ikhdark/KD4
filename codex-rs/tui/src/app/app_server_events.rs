//! App-server event stream handling for the TUI app.

use super::App;
use super::app_server_event_targets::ServerNotificationThreadTarget;
use super::app_server_event_targets::server_notification_thread_target;
use super::app_server_event_targets::server_request_thread_id;
use crate::app_command::AppCommand;
use crate::app_event::AppEvent;
use crate::app_event::ConnectorsSnapshot;
use crate::app_info::app_info_from_api;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::status_account_display_from_auth_mode;
use crate::local_chatgpt_auth::load_local_chatgpt_auth;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::AuthMode;
use codex_app_server_protocol::ChatgptAuthTokensRefreshParams;
use codex_app_server_protocol::ChatgptAuthTokensRefreshResponse;
use codex_app_server_protocol::CurrentTimeReadResponse;
use codex_app_server_protocol::RateLimitReachedType;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

impl App {
    pub(super) fn refresh_mcp_startup_expected_servers_from_config(&mut self) {
        let enabled_config_mcp_servers: Vec<String> = self
            .config
            .mcp_servers
            .get()
            .iter()
            .filter_map(|(name, server)| server.enabled.then_some(name.clone()))
            .collect();
        self.chat_widget
            .set_mcp_startup_expected_servers(enabled_config_mcp_servers);
    }

    pub(super) async fn handle_app_server_event(
        &mut self,
        app_server_client: &AppServerSession,
        event: AppServerEvent,
    ) {
        match event {
            AppServerEvent::Lagged { skipped } => {
                tracing::warn!(
                    skipped,
                    "app-server event consumer lagged; dropping ignored events"
                );
                self.refresh_mcp_startup_expected_servers_from_config();
                self.chat_widget.finish_mcp_startup_after_lag();
            }
            AppServerEvent::ServerNotification(notification) => {
                self.handle_server_notification_event(app_server_client, notification)
                    .await;
            }
            AppServerEvent::ServerRequest(request) => {
                self.handle_server_request_event(app_server_client, request)
                    .await;
            }
            AppServerEvent::Disconnected { message } => {
                tracing::warn!("app-server event stream disconnected: {message}");
                self.chat_widget.add_error_message(message.clone());
                self.app_event_tx.send(AppEvent::FatalExitRequest(message));
            }
        }
    }

    async fn handle_server_notification_event(
        &mut self,
        app_server_client: &AppServerSession,
        notification: ServerNotification,
    ) {
        match &notification {
            ServerNotification::ServerRequestResolved(notification) => {
                if let Some(request) = self
                    .pending_app_server_requests
                    .resolve_notification(&notification.request_id)
                {
                    self.chat_widget.dismiss_app_server_request(&request);
                }
            }
            ServerNotification::McpServerStatusUpdated(_) => {
                self.refresh_mcp_startup_expected_servers_from_config();
            }
            ServerNotification::AccountRateLimitsUpdated(notification) => {
                if matches!(
                    notification.rate_limits.rate_limit_reached_type,
                    Some(
                        RateLimitReachedType::WorkspaceOwnerCreditsDepleted
                            | RateLimitReachedType::WorkspaceMemberCreditsDepleted
                            | RateLimitReachedType::WorkspaceOwnerUsageLimitReached
                            | RateLimitReachedType::WorkspaceMemberUsageLimitReached
                    )
                ) || notification.rate_limits.spend_control_reached == Some(true)
                {
                    self.rate_limit_hard_stop_generation =
                        self.rate_limit_hard_stop_generation.wrapping_add(1);
                }
                self.chat_widget
                    .on_rolling_rate_limit_snapshot(notification.rate_limits.clone());
                return;
            }
            ServerNotification::AccountUpdated(notification) => {
                let has_codex_backend_auth = matches!(
                    notification.auth_mode,
                    Some(
                        AuthMode::Chatgpt
                            | AuthMode::ChatgptAuthTokens
                            | AuthMode::AgentIdentity
                            | AuthMode::PersonalAccessToken
                    )
                );
                self.chat_widget.update_account_state(
                    status_account_display_from_auth_mode(
                        notification.auth_mode,
                        notification.plan_type,
                    ),
                    notification.plan_type,
                    notification
                        .auth_mode
                        .is_some_and(AuthMode::has_chatgpt_account),
                    has_codex_backend_auth,
                );
                return;
            }
            ServerNotification::ExternalAgentConfigImportCompleted(notification) => {
                let should_report_completion =
                    app_server_client.consume_external_agent_config_import_completion();
                if let Err(err) = self.refresh_in_memory_config_from_disk().await {
                    tracing::warn!(
                        error = %err,
                        "failed to refresh config after external agent config import"
                    );
                }
                let cwd = self.chat_widget.config_ref().cwd.to_path_buf();
                self.chat_widget.refresh_plugin_mentions();
                self.chat_widget.submit_op(AppCommand::reload_user_config());
                self.fetch_plugins_list(app_server_client, cwd);
                if should_report_completion {
                    self.chat_widget.add_plain_history_lines(
                        crate::external_agent_config_migration_flow::external_agent_config_migration_finished_lines(notification),
                    );
                }
                return;
            }
            ServerNotification::WindowsSandboxSetupCompleted(notification) => {
                let Some(pending) = self.windows_sandbox.pending_setup.take() else {
                    tracing::warn!(
                        ?notification.mode,
                        "ignoring Windows sandbox setup completion without a pending TUI setup"
                    );
                    return;
                };
                if notification.success {
                    self.app_event_tx
                        .send(AppEvent::EnableWindowsSandboxForAgentMode {
                            preset: pending.preset,
                            mode: pending.mode,
                            profile_selection: pending.profile_selection,
                        });
                } else {
                    let error = notification
                        .error
                        .clone()
                        .unwrap_or_else(|| "Windows sandbox setup failed".to_string());
                    match pending.mode {
                        crate::app_event::WindowsSandboxEnableMode::Elevated => self
                            .app_event_tx
                            .send(AppEvent::OpenWindowsSandboxFallbackPrompt {
                                preset: pending.preset,
                                profile_selection: pending.profile_selection,
                            }),
                        crate::app_event::WindowsSandboxEnableMode::Legacy => self
                            .app_event_tx
                            .send(AppEvent::WindowsSandboxLegacySetupFailed {
                                preset: pending.preset,
                                profile_selection: pending.profile_selection,
                                error,
                            }),
                    }
                }
                return;
            }
            ServerNotification::AppListUpdated(notification) => {
                self.chat_widget.on_connectors_loaded(
                    Ok(ConnectorsSnapshot {
                        connectors: notification
                            .data
                            .iter()
                            .cloned()
                            .map(app_info_from_api)
                            .collect(),
                    }),
                    /*is_final*/ false,
                );
                return;
            }
            _ => {}
        }

        match server_notification_thread_target(&notification) {
            ServerNotificationThreadTarget::Thread(thread_id) => {
                let result = if self.primary_thread_id == Some(thread_id)
                    || self.primary_thread_id.is_none()
                {
                    self.enqueue_primary_thread_notification(notification).await
                } else {
                    self.enqueue_thread_notification(thread_id, notification)
                        .await
                };

                if let Err(err) = result {
                    tracing::warn!("failed to enqueue app-server notification: {err}");
                }
                return;
            }
            ServerNotificationThreadTarget::InvalidThreadId(thread_id) => {
                tracing::warn!(
                    thread_id,
                    "ignoring app-server notification with invalid thread_id"
                );
                return;
            }
            ServerNotificationThreadTarget::AppScoped => {
                tracing::debug!(
                    "ignoring app-scoped MCP startup notification without a TUI app-level target"
                );
                return;
            }
            ServerNotificationThreadTarget::Global => {}
        }

        self.chat_widget
            .handle_server_notification(notification, /*replay_kind*/ None);
    }

    async fn handle_server_request_event(
        &mut self,
        app_server_client: &AppServerSession,
        request: ServerRequest,
    ) {
        if let ServerRequest::ChatgptAuthTokensRefresh { request_id, params } = request {
            self.handle_chatgpt_auth_tokens_refresh_request(app_server_client, request_id, params)
                .await;
            return;
        }

        if let ServerRequest::CurrentTimeRead { request_id, .. } = &request {
            let response = current_time_read_response(SystemTime::now()).and_then(|response| {
                serde_json::to_value(response)
                    .map_err(|err| format!("failed to serialize current time response: {err}"))
            });
            match response {
                Ok(response) => {
                    if let Err(err) = app_server_client
                        .resolve_server_request(request_id.clone(), response)
                        .await
                    {
                        tracing::warn!("failed to resolve current time request: {err}");
                    }
                }
                Err(message) => {
                    self.chat_widget.add_error_message(message.clone());
                    if let Err(err) = self
                        .reject_app_server_request(app_server_client, request_id.clone(), message)
                        .await
                    {
                        tracing::warn!("{err}");
                    }
                }
            }
            return;
        }

        if let Some(unsupported) = self
            .pending_app_server_requests
            .note_server_request(&request)
        {
            tracing::warn!(
                request_id = ?unsupported.request_id,
                message = unsupported.message,
                "rejecting unsupported app-server request"
            );
            self.chat_widget
                .add_error_message(unsupported.message.clone());
            if let Err(err) = self
                .reject_app_server_request(
                    app_server_client,
                    unsupported.request_id,
                    unsupported.message,
                )
                .await
            {
                tracing::warn!("{err}");
            }
            return;
        }

        let Some(thread_id) = server_request_thread_id(&request) else {
            tracing::warn!("ignoring threadless app-server request");
            return;
        };

        let result =
            if self.primary_thread_id == Some(thread_id) || self.primary_thread_id.is_none() {
                self.enqueue_primary_thread_request(request).await
            } else {
                self.enqueue_thread_request(thread_id, request).await
            };
        if let Err(err) = result {
            tracing::warn!("failed to enqueue app-server request: {err}");
        }
    }

    async fn handle_chatgpt_auth_tokens_refresh_request(
        &mut self,
        app_server_client: &AppServerSession,
        request_id: codex_app_server_protocol::RequestId,
        params: ChatgptAuthTokensRefreshParams,
    ) {
        let config = self.config.clone();
        let result = tokio::task::spawn_blocking(move || {
            let auth = load_local_chatgpt_auth(
                &config.codex_home,
                config.cli_auth_credentials_store_mode,
                config.forced_chatgpt_workspace_id.as_deref(),
            )?;
            chatgpt_auth_tokens_refresh_response(&auth, &params)
        })
        .await;

        let response = match result {
            Ok(Ok(response)) => serde_json::to_value(response)
                .map_err(|err| format!("failed to serialize ChatGPT auth refresh response: {err}")),
            Ok(Err(err)) => Err(err),
            Err(err) => Err(format!("ChatGPT auth refresh task failed: {err}")),
        };

        match response {
            Ok(response) => {
                if let Err(err) = app_server_client
                    .resolve_server_request(request_id, response)
                    .await
                {
                    tracing::warn!("failed to resolve ChatGPT auth refresh request: {err}");
                }
            }
            Err(message) => {
                self.chat_widget.add_error_message(message.clone());
                if let Err(err) = self
                    .reject_app_server_request(app_server_client, request_id, message)
                    .await
                {
                    tracing::warn!("{err}");
                }
            }
        }
    }
}

fn current_time_read_response(now: SystemTime) -> Result<CurrentTimeReadResponse, String> {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("system time is before the Unix epoch: {err}"))?
        .as_secs();
    let current_time_at = i64::try_from(seconds)
        .map_err(|_| "current Unix time does not fit in an i64".to_string())?;
    Ok(CurrentTimeReadResponse { current_time_at })
}

fn chatgpt_auth_tokens_refresh_response(
    auth: &crate::local_chatgpt_auth::LocalChatgptAuth,
    params: &ChatgptAuthTokensRefreshParams,
) -> Result<ChatgptAuthTokensRefreshResponse, String> {
    if let Some(previous_account_id) = params.previous_account_id.as_deref()
        && previous_account_id != auth.chatgpt_account_id
    {
        return Err(format!(
            "local ChatGPT auth refresh account mismatch: expected `{previous_account_id}`, got `{}`",
            auth.chatgpt_account_id
        ));
    }

    Ok(auth.to_refresh_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_chatgpt_auth::LocalChatgptAuth;
    use codex_app_server_protocol::ChatgptAuthTokensRefreshReason;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    fn refresh_params(previous_account_id: Option<&str>) -> ChatgptAuthTokensRefreshParams {
        ChatgptAuthTokensRefreshParams {
            reason: ChatgptAuthTokensRefreshReason::Unauthorized,
            previous_account_id: previous_account_id.map(str::to_string),
        }
    }

    fn local_auth() -> LocalChatgptAuth {
        LocalChatgptAuth {
            access_token: "access-token".to_string(),
            chatgpt_account_id: "workspace-1".to_string(),
            chatgpt_plan_type: Some("business".to_string()),
        }
    }

    #[test]
    fn current_time_response_uses_whole_unix_seconds() {
        let response = current_time_read_response(UNIX_EPOCH + Duration::from_millis(12_345))
            .expect("post-epoch time should produce a response");

        assert_eq!(response.current_time_at, 12);
    }

    #[test]
    fn chatgpt_auth_refresh_returns_current_local_credentials() {
        let response = chatgpt_auth_tokens_refresh_response(
            &local_auth(),
            &refresh_params(Some("workspace-1")),
        )
        .expect("matching local credentials should resolve the refresh request");

        assert_eq!(
            response,
            ChatgptAuthTokensRefreshResponse {
                access_token: "access-token".to_string(),
                chatgpt_account_id: "workspace-1".to_string(),
                chatgpt_plan_type: Some("business".to_string()),
            }
        );
    }

    #[test]
    fn chatgpt_auth_refresh_rejects_account_changes() {
        let error = chatgpt_auth_tokens_refresh_response(
            &local_auth(),
            &refresh_params(Some("workspace-2")),
        )
        .expect_err("a refresh must not switch ChatGPT accounts");

        assert_eq!(
            error,
            "local ChatGPT auth refresh account mismatch: expected `workspace-2`, got `workspace-1`"
        );
    }
}
