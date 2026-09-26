use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use codex_builtin_extensions::BuiltinExtensionDependencies;
use codex_builtin_extensions::GoalService;
use codex_builtin_extensions::install_builtin_extensions;
use codex_core::StateDbHandle;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_home::CodexHomeUserInstructionsProvider;
use codex_login::AuthManager;
use codex_login::default_client::USER_AGENT_SUFFIX;
use codex_login::default_client::get_codex_user_agent;
use codex_protocol::protocol::SessionSource;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::ClientNotification;
use rmcp::model::ClientRequest;
use rmcp::model::ErrorCode;
use rmcp::model::ErrorData;
use rmcp::model::Implementation;
use rmcp::model::InitializeResult;
use rmcp::model::JsonRpcError;
use rmcp::model::JsonRpcNotification;
use rmcp::model::JsonRpcRequest;
use rmcp::model::JsonRpcResponse;
use rmcp::model::ProtocolVersion;
use rmcp::model::RequestId;
use rmcp::model::ServerCapabilities;
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::codex_tool_config::CodexToolCallParam;
use crate::codex_tool_config::CodexToolCallReplyParam;
use crate::codex_tool_config::ToolSessionDefaults;
use crate::codex_tool_config::create_tool_for_codex_tool_call_param;
use crate::codex_tool_config::create_tool_for_codex_tool_call_reply_param;
use crate::codex_tool_runner::RunningRequest;
use crate::outgoing_message::OutgoingMessageSender;

pub(crate) struct MessageProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    initialized: bool,
    session_defaults: Arc<ToolSessionDefaults>,
    thread_manager: Arc<ThreadManager>,
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, RunningRequest>>>,
    tool_tasks: ToolTasks,
}

#[derive(Default)]
struct ToolTasks {
    cancellation_token: CancellationToken,
    tasks: TaskTracker,
}

fn mcp_extension_registry(
    dependencies: BuiltinExtensionDependencies,
) -> Arc<ExtensionRegistry<Config>> {
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install_builtin_extensions(&mut extensions, dependencies);
    Arc::new(extensions.build())
}

impl ToolTasks {
    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let cancellation_token = self.cancellation_token.clone();
        self.tasks.spawn(async move {
            tokio::select! {
                _ = cancellation_token.cancelled() => {}
                _ = task => {}
            }
        });
    }

    async fn shutdown(&self) {
        self.cancellation_token.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }
}

impl MessageProcessor {
    /// Create a new `MessageProcessor`, retaining a handle to the outgoing
    /// `Sender` so handlers can enqueue messages to be written to stdout.
    pub(crate) async fn new(
        outgoing: OutgoingMessageSender,
        session_defaults: ToolSessionDefaults,
        config: Arc<Config>,
        environment_manager: Arc<EnvironmentManager>,
        state_db: Option<StateDbHandle>,
        installation_id: String,
    ) -> Self {
        let outgoing = Arc::new(outgoing);
        // Tool sessions share the server's home rather than resolving it again.
        let session_defaults = Arc::new(ToolSessionDefaults {
            codex_home: Some(config.codex_home.to_path_buf()),
            ..session_defaults
        });
        let auth_manager = AuthManager::shared_from_config(
            config.as_ref(),
            /*enable_codex_api_key_env*/ false,
        )
        .await;
        let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
            config.codex_home.clone(),
        ));
        let thread_store = codex_core::thread_store_from_config(config.as_ref(), state_db.clone());
        let goal_service = Arc::new(GoalService::new());
        let environment_manager_for_extensions = Arc::clone(&environment_manager);
        let thread_manager = Arc::new_cyclic(|thread_manager| {
            let extensions = mcp_extension_registry(BuiltinExtensionDependencies {
                auth_manager: auth_manager.clone(),
                state_db: state_db.clone(),
                analytics_events_client: None,
                thread_manager: thread_manager.clone(),
                goal_service: Arc::clone(&goal_service),
                environment_manager: Arc::clone(&environment_manager_for_extensions),
                session_source: SessionSource::Mcp,
            });
            ThreadManager::new(
                config.as_ref(),
                auth_manager,
                SessionSource::Mcp,
                environment_manager,
                extensions,
                user_instructions_provider,
                /*analytics_events_client*/ None,
                Arc::clone(&thread_store),
                codex_core::local_agent_graph_store_from_state_db(state_db.as_ref()),
                installation_id,
                /*attestation_provider*/ None,
                /*external_time_provider*/ None,
            )
        });
        Self {
            outgoing,
            initialized: false,
            session_defaults,
            thread_manager,
            running_requests_id_to_codex_uuid: Arc::new(Mutex::new(HashMap::new())),
            tool_tasks: ToolTasks::default(),
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.tool_tasks.shutdown().await;
        self.outgoing.cancel_all_requests().await;
        let report = self
            .thread_manager
            .shutdown_all_threads_bounded(Duration::from_secs(10))
            .await;
        for thread_id in report.submit_failed {
            tracing::warn!(%thread_id, "failed to submit Shutdown to MCP thread");
        }
        for thread_id in report.timed_out {
            tracing::warn!(%thread_id, "timed out shutting down MCP thread");
        }
        self.running_requests_id_to_codex_uuid.lock().await.clear();
    }

    pub(crate) async fn process_request(&mut self, request: JsonRpcRequest<ClientRequest>) {
        let request_id = request.id.clone();
        if self
            .running_requests_id_to_codex_uuid
            .lock()
            .await
            .contains_key(&request_id)
        {
            self.outgoing
                .send_error(
                    request_id,
                    ErrorData::new(
                        ErrorCode::INVALID_REQUEST,
                        "A request with this ID is still running.",
                        None,
                    ),
                )
                .await;
            return;
        }
        let client_request = request.request;

        match client_request {
            ClientRequest::InitializeRequest(params) => {
                self.handle_initialize(request_id, params.params).await;
            }
            ClientRequest::PingRequest(_params) => {
                self.handle_ping(request_id).await;
            }
            ClientRequest::ListResourcesRequest(_) => {
                self.handle_unsupported_request(request_id, "resources/list")
                    .await;
            }
            ClientRequest::ListResourceTemplatesRequest(_) => {
                self.handle_unsupported_request(request_id, "resources/templates/list")
                    .await;
            }
            ClientRequest::ReadResourceRequest(_) => {
                self.handle_unsupported_request(request_id, "resources/read")
                    .await;
            }
            ClientRequest::SubscribeRequest(_) => {
                self.handle_unsupported_request(request_id, "resources/subscribe")
                    .await;
            }
            ClientRequest::UnsubscribeRequest(_) => {
                self.handle_unsupported_request(request_id, "resources/unsubscribe")
                    .await;
            }
            ClientRequest::ListPromptsRequest(_) => {
                self.handle_unsupported_request(request_id, "prompts/list")
                    .await;
            }
            ClientRequest::GetPromptRequest(_) => {
                self.handle_unsupported_request(request_id, "prompts/get")
                    .await;
            }
            ClientRequest::ListToolsRequest(params) => {
                self.handle_list_tools(request_id, params.params).await;
            }
            ClientRequest::CallToolRequest(params) => {
                self.handle_call_tool(request_id, params.params).await;
            }
            ClientRequest::SetLevelRequest(_) => {
                self.handle_unsupported_request(request_id, "logging/setLevel")
                    .await;
            }
            ClientRequest::CompleteRequest(_) => {
                self.handle_unsupported_request(request_id, "completion/complete")
                    .await;
            }
            ClientRequest::GetTaskInfoRequest(_) => {
                self.handle_unsupported_request(request_id, "tasks/get_info")
                    .await;
            }
            ClientRequest::ListTasksRequest(_) => {
                self.handle_unsupported_request(request_id, "tasks/list")
                    .await;
            }
            ClientRequest::GetTaskResultRequest(_) => {
                self.handle_unsupported_request(request_id, "tasks/get_result")
                    .await;
            }
            ClientRequest::CancelTaskRequest(_) => {
                self.handle_unsupported_request(request_id, "tasks/cancel")
                    .await;
            }
            ClientRequest::CustomRequest(custom) => {
                let method = custom.method.clone();
                self.outgoing
                    .send_error(
                        request_id,
                        ErrorData::new(
                            ErrorCode::METHOD_NOT_FOUND,
                            format!("method not found: {method}"),
                            Some(json!({ "method": method })),
                        ),
                    )
                    .await;
            }
        }
    }

    pub(crate) async fn process_response(&mut self, response: JsonRpcResponse<serde_json::Value>) {
        tracing::info!("<- response id={:?}", response.id);
        let JsonRpcResponse { id, result, .. } = response;
        self.outgoing.notify_client_response(id, result).await
    }

    pub(crate) async fn process_notification(
        &mut self,
        notification: JsonRpcNotification<ClientNotification>,
    ) {
        match notification.notification {
            ClientNotification::CancelledNotification(params) => {
                self.handle_cancelled_notification(params.params).await;
            }
            ClientNotification::ProgressNotification(params) => {
                self.handle_progress_notification(params.params);
            }
            ClientNotification::RootsListChangedNotification(_params) => {
                self.handle_roots_list_changed();
            }
            ClientNotification::InitializedNotification(_) => {
                self.handle_initialized_notification();
            }
            ClientNotification::CustomNotification(_) => {
                tracing::warn!("ignoring custom client notification");
            }
        }
    }

    pub(crate) async fn process_error(&mut self, err: JsonRpcError) {
        tracing::error!("<- client error id={:?}", err.id);
        if let Some(id) = err.id {
            self.outgoing.cancel_request(&id).await;
        }
    }

    async fn handle_initialize(
        &mut self,
        id: RequestId,
        params: rmcp::model::InitializeRequestParams,
    ) {
        tracing::info!("initialize");

        if self.initialized {
            self.outgoing
                .send_error(
                    id,
                    ErrorData::invalid_request("initialize called more than once", None),
                )
                .await;
            return;
        }

        let elicitation = params.capabilities.elicitation.as_ref();
        self.outgoing.set_elicitation_capabilities(
            elicitation
                .is_some_and(|capability| capability.form.is_some() || capability.url.is_none()),
            elicitation
                .and_then(|capability| capability.url.as_ref())
                .is_some(),
        );
        let client_info = params.client_info;
        let name = client_info.name;
        let version = client_info.version;
        let user_agent_suffix = format!("{name}; {version}");
        if let Ok(mut suffix) = USER_AGENT_SUFFIX.lock() {
            *suffix = Some(user_agent_suffix);
        }

        let server_info =
            Implementation::new("codex-mcp-server", env!("CARGO_PKG_VERSION")).with_title("Codex");

        // Preserve Codex's existing non-spec `serverInfo.user_agent` field.
        let mut server_info_value = match serde_json::to_value(&server_info) {
            Ok(value) => value,
            Err(err) => {
                self.outgoing
                    .send_error(
                        id,
                        ErrorData::internal_error(
                            format!("failed to serialize server info: {err}"),
                            None,
                        ),
                    )
                    .await;
                return;
            }
        };
        if let serde_json::Value::Object(ref mut obj) = server_info_value {
            obj.insert("user_agent".to_string(), json!(get_codex_user_agent()));
        }

        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        let protocol_version = if [
            ProtocolVersion::V_2024_11_05,
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_11_25,
        ]
        .contains(&params.protocol_version)
        {
            params.protocol_version
        } else {
            ProtocolVersion::V_2025_11_25
        };
        let result = InitializeResult::new(capabilities)
            .with_protocol_version(protocol_version)
            .with_server_info(server_info);
        let mut result_value = match serde_json::to_value(result) {
            Ok(value) => value,
            Err(err) => {
                self.outgoing
                    .send_error(
                        id,
                        ErrorData::internal_error(
                            format!("failed to serialize initialize response: {err}"),
                            None,
                        ),
                    )
                    .await;
                return;
            }
        };

        if let serde_json::Value::Object(ref mut obj) = result_value {
            obj.insert("serverInfo".to_string(), server_info_value);
        }

        self.initialized = true;
        self.outgoing.send_response(id, result_value).await;
    }

    async fn handle_ping(&self, id: RequestId) {
        tracing::info!("ping");
        self.outgoing.send_response(id, json!({})).await;
    }

    async fn handle_list_tools(
        &self,
        id: RequestId,
        _params: Option<rmcp::model::PaginatedRequestParams>,
    ) {
        tracing::trace!("tools/list");
        let result = rmcp::model::ListToolsResult {
            meta: None,
            tools: vec![
                create_tool_for_codex_tool_call_param(),
                create_tool_for_codex_tool_call_reply_param(),
            ],
            next_cursor: None,
        };

        self.outgoing.send_response(id, result).await;
    }

    async fn handle_call_tool(&self, id: RequestId, params: CallToolRequestParams) {
        tracing::info!("tools/call name={}", params.name);
        let CallToolRequestParams {
            name, arguments, ..
        } = params;

        match name.as_ref() {
            "codex" => self.handle_tool_call_codex(id, arguments).await,
            "codex-reply" => {
                self.handle_tool_call_codex_session_reply(id, arguments)
                    .await
            }
            _ => {
                let result = CallToolResult::error(vec![rmcp::model::Content::text(format!(
                    "Unknown tool '{name}'"
                ))]);
                self.outgoing.send_response(id, result).await;
            }
        }
    }

    async fn handle_tool_call_codex(
        &self,
        id: RequestId,
        arguments: Option<rmcp::model::JsonObject>,
    ) {
        let arguments = arguments.map(serde_json::Value::Object);
        let tool_cfg = match arguments {
            Some(json_val) => match serde_json::from_value::<CodexToolCallParam>(json_val) {
                Ok(tool_cfg) => tool_cfg,
                Err(e) => {
                    let result = CallToolResult::error(vec![rmcp::model::Content::text(format!(
                        "Failed to parse configuration for Codex tool: {e}"
                    ))]);
                    self.outgoing.send_response(id, result).await;
                    return;
                }
            },
            None => {
                let result = CallToolResult::error(vec![rmcp::model::Content::text(
                    "Missing arguments for codex tool-call; the `prompt` field is required.",
                )]);
                self.outgoing.send_response(id, result).await;
                return;
            }
        };

        let request = RunningRequest {
            thread_id: None,
            turn_id: id.to_string(),
            cancellation: CancellationToken::new(),
        };
        self.running_requests_id_to_codex_uuid
            .lock()
            .await
            .insert(id.clone(), request.clone());
        let session_defaults = Arc::clone(&self.session_defaults);

        // Clone outgoing and server to move into async task.
        let outgoing = self.outgoing.clone();
        let thread_manager = self.thread_manager.clone();
        let running_requests_id_to_codex_uuid = self.running_requests_id_to_codex_uuid.clone();

        // Spawn an async task to handle the Codex session so that we do not
        // block the synchronous message-processing loop.
        self.tool_tasks.spawn(async move {
            let prepared = tokio::select! {
                biased;
                _ = request.cancellation.cancelled() => Err("Codex request cancelled during startup.".to_string()),
                result = tool_cfg.into_config(&session_defaults) => result.map_err(|e| format!("Failed to load Codex configuration from overrides: {e}")),
            };
            let (initial_prompt, config) = match prepared {
                Ok(prepared) => prepared,
                Err(message) => {
                    outgoing.send_response(id.clone(), CallToolResult::error(vec![rmcp::model::Content::text(message)])).await;
                    running_requests_id_to_codex_uuid.lock().await.remove(&id);
                    return;
                }
            };
            crate::codex_tool_runner::run_codex_tool_session(
                id,
                initial_prompt,
                config,
                outgoing,
                thread_manager,
                request,
                running_requests_id_to_codex_uuid,
            )
            .await;
        });
    }

    async fn handle_tool_call_codex_session_reply(
        &self,
        request_id: RequestId,
        arguments: Option<rmcp::model::JsonObject>,
    ) {
        let arguments = arguments.map(serde_json::Value::Object);
        tracing::info!("tools/call codex-reply");

        // parse arguments
        let codex_tool_call_reply_param: CodexToolCallReplyParam = match arguments {
            Some(json_val) => match serde_json::from_value::<CodexToolCallReplyParam>(json_val) {
                Ok(params) => params,
                Err(e) => {
                    tracing::error!("Failed to parse Codex tool call reply parameters: {e}");
                    let result = CallToolResult::error(vec![rmcp::model::Content::text(format!(
                        "Failed to parse configuration for Codex tool: {e}"
                    ))]);
                    self.outgoing.send_response(request_id, result).await;
                    return;
                }
            },
            None => {
                tracing::error!(
                    "Missing arguments for codex-reply tool-call; the `threadId` (or `conversationId`) and `prompt` fields are required."
                );
                let result = CallToolResult::error(vec![rmcp::model::Content::text(
                    "Missing arguments for codex-reply tool-call; the `threadId` (or `conversationId`) and `prompt` fields are required.",
                )]);
                self.outgoing.send_response(request_id, result).await;
                return;
            }
        };

        let thread_id = match codex_tool_call_reply_param.get_thread_id() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to parse thread_id: {e}");
                let result = CallToolResult::error(vec![rmcp::model::Content::text(format!(
                    "Failed to parse thread_id: {e}"
                ))]);
                self.outgoing.send_response(request_id, result).await;
                return;
            }
        };

        // Clone outgoing to move into async task.
        let outgoing = self.outgoing.clone();
        let running_requests_id_to_codex_uuid = self.running_requests_id_to_codex_uuid.clone();

        let codex = match self.thread_manager.get_thread(thread_id).await {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!("Session not found for thread_id: {thread_id}");
                let result = crate::codex_tool_runner::create_call_tool_result_with_thread_id(
                    thread_id,
                    format!("Session not found for thread_id: {thread_id}"),
                    Some(true),
                );
                outgoing.send_response(request_id, result).await;
                return;
            }
        };

        // A core turn may already be complete while its runner is still draining
        // the shared event queue. Keep exclusive ownership until that runner exits.
        let request = RunningRequest {
            thread_id: Some(thread_id),
            turn_id: codex.reserve_turn_id(),
            cancellation: CancellationToken::new(),
        };
        {
            let mut requests = running_requests_id_to_codex_uuid.lock().await;
            if requests
                .values()
                .any(|request| request.thread_id == Some(thread_id))
            {
                drop(requests);
                self.tool_tasks.spawn(async move {
                    outgoing.send_response(request_id, crate::codex_tool_runner::create_call_tool_result_with_thread_id(
                        thread_id, "A Codex request is still running for this thread; wait for its response before replying.".to_string(), Some(true),
                    )).await;
                });
                return;
            }
            requests.insert(request_id.clone(), request.clone());
        }

        // Spawn the long-running reply handler.
        let prompt = codex_tool_call_reply_param.prompt.clone();
        self.tool_tasks.spawn({
            let outgoing = outgoing.clone();
            let running_requests_id_to_codex_uuid = running_requests_id_to_codex_uuid.clone();

            async move {
                crate::codex_tool_runner::run_codex_tool_session_reply(
                    thread_id,
                    codex,
                    outgoing,
                    request_id,
                    prompt,
                    request,
                    running_requests_id_to_codex_uuid,
                )
                .await;
            }
        });
    }

    async fn handle_unsupported_request(&self, id: RequestId, method: &str) {
        self.outgoing
            .send_error(
                id,
                ErrorData::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("method not found: {method}"),
                    Some(json!({ "method": method })),
                ),
            )
            .await;
    }

    // ---------------------------------------------------------------------
    // Notification handlers
    // ---------------------------------------------------------------------

    async fn handle_cancelled_notification(&self, params: rmcp::model::CancelledNotificationParam) {
        let request_id = params.request_id;
        // Create a stable string form early for logging and submission id.
        let request_id_string = request_id.to_string();

        // Obtain the thread id while holding the first lock, then release.
        let request = {
            let map_guard = self.running_requests_id_to_codex_uuid.lock().await;
            match map_guard.get(&request_id) {
                Some(request) => request.clone(),
                None => {
                    tracing::warn!("Session not found for request_id: {request_id_string}");
                    return;
                }
            }
        };
        // Preserve cancellation while the normal turn-start admission is pending.
        request.cancellation.cancel();
        let Some(thread_id) = request.thread_id else {
            return;
        };
        tracing::info!("thread_id: {thread_id}");

        // Obtain the Codex thread from the server.
        let codex_arc = match self.thread_manager.get_thread(thread_id).await {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!("Session not found for thread_id: {thread_id}");
                return;
            }
        };

        // Core checks this identity while claiming the active turn's terminal
        // transition, so completion/new-turn races cannot redirect cancellation.
        codex_arc.interrupt_turn_if_active(&request.turn_id).await;
        // The runner retains event ownership until it has sent the final response.
    }

    fn handle_progress_notification(&self, _params: rmcp::model::ProgressNotificationParam) {
        tracing::info!("notifications/progress");
    }

    fn handle_roots_list_changed(&self) {
        tracing::info!("notifications/roots/list_changed");
    }

    fn handle_initialized_notification(&self) {
        tracing::info!("notifications/initialized");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Weak;

    use codex_exec_server::EnvironmentManager;
    use codex_login::CodexAuth;
    use pretty_assertions::assert_eq;

    use super::*;

    async fn test_processor() -> anyhow::Result<(
        tempfile::TempDir,
        MessageProcessor,
        tokio::sync::mpsc::Receiver<crate::outgoing_message::OutgoingMessage>,
    )> {
        let home = tempfile::TempDir::new()?;
        let config = codex_core::config::ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let processor = MessageProcessor::new(
            OutgoingMessageSender::new(tx),
            ToolSessionDefaults::default(),
            Arc::new(config),
            Arc::new(EnvironmentManager::default_for_tests()),
            None,
            "test".into(),
        )
        .await;
        Ok((home, processor, rx))
    }

    #[tokio::test]
    async fn initialize_recognizes_legacy_and_explicit_form_capabilities() -> anyhow::Result<()> {
        for (capabilities, expected) in [
            (json!({}), false),
            (json!({"elicitation":{}}), true),
            (json!({"elicitation":{"url":{}}}), false),
            (json!({"elicitation":{"form":{}}}), true),
        ] {
            let (_home, mut processor, mut rx) = test_processor().await?;
            processor
                .process_request(serde_json::from_value(json!({
                    "jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
                        "protocolVersion":"2025-03-26", "capabilities":capabilities,
                        "clientInfo":{"name":"test","version":"1"}
                    }
                }))?)
                .await;
            let Some(crate::outgoing_message::OutgoingMessage::Response(response)) =
                rx.recv().await
            else {
                panic!("expected initialize response");
            };
            assert_eq!(response.result["protocolVersion"], "2025-03-26");
            assert_eq!(processor.outgoing.supports_form_elicitation(), expected);
            processor.shutdown().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_request_id_preserves_running_request_cancellation() -> anyhow::Result<()> {
        for duplicate_name in ["codex", "codex-reply"] {
            let (_home, mut processor, mut rx) = test_processor().await?;
            let request = |name| {
                serde_json::from_value(json!({
                    "jsonrpc":"2.0", "id":1, "method":"tools/call",
                    "params":{"name":name, "arguments":{"prompt":"must not start"}}
                }))
            };
            processor.process_request(request("codex")?).await;
            let original_cancellation = processor
                .running_requests_id_to_codex_uuid
                .lock()
                .await
                .get(&RequestId::Number(1))
                .expect("normal request admission registers its cancellation owner")
                .cancellation
                .clone();
            processor.process_request(request(duplicate_name)?).await;
            let Some(crate::outgoing_message::OutgoingMessage::Error(error)) = rx.recv().await
            else {
                panic!("duplicate admission must fail before starting a worker");
            };
            assert_eq!(error.id, Some(RequestId::Number(1)));
            assert_eq!(error.error.code, ErrorCode::INVALID_REQUEST);
            assert_eq!(
                error.error.message,
                "A request with this ID is still running."
            );
            assert!(!original_cancellation.is_cancelled());
            processor
                .process_notification(serde_json::from_value(json!({
                    "jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":1}
                }))?)
                .await;
            assert!(
                original_cancellation.is_cancelled(),
                "cancellation must still reach the first admitted request"
            );
            let response = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await?;
            let Some(crate::outgoing_message::OutgoingMessage::Response(response)) = response
            else {
                panic!("only the cancelled original request should publish a result");
            };
            assert_eq!(response.result["isError"], true);
            assert_eq!(
                response.result["content"][0]["text"],
                "Codex request cancelled during startup."
            );
            processor.shutdown().await;
            assert!(
                processor
                    .running_requests_id_to_codex_uuid
                    .lock()
                    .await
                    .is_empty()
            );
            assert!(
                rx.try_recv().is_err(),
                "the rejected request must not start another worker"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_before_worker_start_is_not_lost() -> anyhow::Result<()> {
        let (_home, mut processor, mut rx) = test_processor().await?;
        processor
            .process_request(serde_json::from_value(json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"codex", "arguments":{"prompt":"must not start"}}
            }))?)
            .await;
        processor
            .process_notification(serde_json::from_value(json!({
                "jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":1}
            }))?)
            .await;
        let response = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await?;
        let Some(crate::outgoing_message::OutgoingMessage::Response(response)) = response else {
            panic!("cancelled startup must respond without session events");
        };
        assert_eq!(response.id, RequestId::Number(1));
        assert_eq!(response.result["isError"], true);
        assert_eq!(
            response.result["content"][0]["text"],
            "Codex request cancelled during startup."
        );
        assert!(
            processor
                .running_requests_id_to_codex_uuid
                .lock()
                .await
                .is_empty()
        );
        processor.shutdown().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stale_mcp_cancellation_leaves_newer_turn_running() -> anyhow::Result<()> {
        use codex_protocol::protocol::AgentStatus;
        use core_test_support::responses::ev_assistant_message;
        use core_test_support::responses::ev_completed;
        use core_test_support::responses::ev_response_created;
        use core_test_support::responses::sse;
        use core_test_support::streaming_sse::StreamingSseChunk;
        use core_test_support::streaming_sse::start_streaming_sse_server;
        use core_test_support::test_codex::test_codex;

        let (second_gate, second_rx) = tokio::sync::oneshot::channel();
        let (server, _completions) = start_streaming_sse_server(vec![
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("first-response"),
                    ev_assistant_message("first-message", "first complete"),
                    ev_completed("first-response"),
                ]),
            }],
            vec![StreamingSseChunk {
                gate: Some(second_rx),
                body: sse(vec![
                    ev_response_created("second-response"),
                    ev_assistant_message("second-message", "second complete"),
                    ev_completed("second-response"),
                ]),
            }],
        ])
        .await;
        let test = test_codex().build_with_streaming_server(&server).await?;
        let thread_id = test.session_configured.thread_id;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        // A slow transport blocks notification publication while the real core
        // turn completes. No request-registry entries are fabricated by the test.
        let output_permit = tx.clone().reserve_owned().await?;
        let mut processor = MessageProcessor {
            outgoing: Arc::new(OutgoingMessageSender::new(tx)),
            initialized: true,
            session_defaults: Arc::default(),
            thread_manager: Arc::clone(&test.thread_manager),
            running_requests_id_to_codex_uuid: Arc::new(Mutex::new(HashMap::new())),
            tool_tasks: ToolTasks::default(),
        };
        let reply = |id, prompt| {
            serde_json::from_value(json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": "codex-reply", "arguments": {
                    "threadId": thread_id.to_string(), "prompt": prompt,
                }},
            }))
        };
        processor.process_request(reply(101, "first prompt")?).await;
        tokio::time::timeout(Duration::from_secs(60), async {
            server.wait_for_request_count(1).await;
            loop {
                if matches!(test.codex.agent_status().await, AgentStatus::Completed(_)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(
            processor
                .running_requests_id_to_codex_uuid
                .lock()
                .await
                .contains_key(&RequestId::Number(101)),
            "blocked completion publication keeps the first MCP request registered"
        );
        processor
            .process_request(reply(102, "second prompt")?)
            .await;
        drop(output_permit);
        let responses = tokio::time::timeout(Duration::from_secs(60), async {
            let mut responses = HashMap::new();
            while responses.len() < 2 {
                if let Some(crate::outgoing_message::OutgoingMessage::Response(response)) =
                    rx.recv().await
                {
                    responses.insert(response.id, response.result);
                }
            }
            responses
        })
        .await?;
        assert_eq!(
            responses[&RequestId::Number(101)]["content"][0]["text"],
            "first complete"
        );
        assert_eq!(responses[&RequestId::Number(102)]["isError"], true);
        assert!(
            responses[&RequestId::Number(102)]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("still running")
        );
        assert_eq!(server.requests().await.len(), 1);
        tokio::time::timeout(Duration::from_secs(5), async {
            while processor
                .running_requests_id_to_codex_uuid
                .lock()
                .await
                .contains_key(&RequestId::Number(101))
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;

        processor
            .process_request(reply(103, "second prompt")?)
            .await;
        tokio::time::timeout(Duration::from_secs(60), server.wait_for_request_count(2)).await?;
        processor
            .process_notification(serde_json::from_value(json!({
                "jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": { "requestId": 101 },
            }))?)
            .await;
        assert_eq!(test.codex.agent_status().await, AgentStatus::Running);
        assert!(
            processor
                .running_requests_id_to_codex_uuid
                .lock()
                .await
                .contains_key(&RequestId::Number(103))
        );
        processor
            .process_request(reply(104, "overlapping prompt")?)
            .await;
        let rejected = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if let Some(crate::outgoing_message::OutgoingMessage::Response(response)) =
                    rx.recv().await
                    && response.id == RequestId::Number(104)
                {
                    break response.result;
                }
            }
        })
        .await?;
        assert_eq!(rejected["isError"], true);
        assert!(
            rejected["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("still running")
        );
        assert_eq!(server.requests().await.len(), 2);
        second_gate
            .send(())
            .expect("the newer model response must still be waiting");
        let outcome = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if let Some(crate::outgoing_message::OutgoingMessage::Response(response)) =
                    rx.recv().await
                    && response.id == RequestId::Number(103)
                {
                    break response.result;
                }
            }
        })
        .await?;
        assert_eq!(outcome["content"][0]["text"], "second complete");
        assert_ne!(outcome["isError"], true);
        processor.shutdown().await;
        server.shutdown().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_turn_result_is_not_consumed_by_the_next_reply() -> anyhow::Result<()> {
        use core_test_support::responses::ev_assistant_message;
        use core_test_support::responses::ev_completed;
        use core_test_support::responses::ev_response_created;
        use core_test_support::responses::sse;
        use core_test_support::responses::sse_failed;
        use core_test_support::streaming_sse::StreamingSseChunk;
        use core_test_support::streaming_sse::start_streaming_sse_server;
        use core_test_support::test_codex::test_codex;

        let (server, _completions) = start_streaming_sse_server(vec![
            vec![StreamingSseChunk {
                gate: None,
                body: sse_failed("failed-response", "insufficient_quota", "quota exhausted"),
            }],
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("second-response"),
                    ev_assistant_message("second-message", "second complete"),
                    ev_completed("second-response"),
                ]),
            }],
        ])
        .await;
        let test = test_codex().build_with_streaming_server(&server).await?;
        let thread_id = test.session_configured.thread_id;
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        let mut processor = MessageProcessor {
            outgoing: Arc::new(OutgoingMessageSender::new(tx)),
            initialized: true,
            session_defaults: Arc::default(),
            thread_manager: Arc::clone(&test.thread_manager),
            running_requests_id_to_codex_uuid: Arc::new(Mutex::new(HashMap::new())),
            tool_tasks: ToolTasks::default(),
        };
        let reply = |id, prompt| {
            serde_json::from_value(json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": "codex-reply", "arguments": {
                    "threadId": thread_id.to_string(), "prompt": prompt,
                }},
            }))
        };
        async fn response_for(
            rx: &mut tokio::sync::mpsc::Receiver<crate::outgoing_message::OutgoingMessage>,
            id: i64,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    if let Some(crate::outgoing_message::OutgoingMessage::Response(response)) =
                        rx.recv().await
                        && response.id == RequestId::Number(id)
                    {
                        break response.result;
                    }
                }
            })
            .await?)
        }

        processor.process_request(reply(201, "first prompt")?).await;
        let failed = response_for(&mut rx, 201).await?;
        assert_eq!(failed["isError"], true);
        assert!(
            failed["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Quota exceeded"),
            "the terminal turn error must be the result: {failed}"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while !processor
                .running_requests_id_to_codex_uuid
                .lock()
                .await
                .is_empty()
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;

        processor
            .process_request(reply(202, "second prompt")?)
            .await;
        let second = response_for(&mut rx, 202).await?;
        assert_eq!(second["content"][0]["text"], "second complete");
        assert_ne!(second["isError"], true);
        assert_eq!(server.requests().await.len(), 2);
        processor.shutdown().await;
        server.shutdown().await;
        Ok(())
    }

    #[test]
    fn mcp_host_uses_the_complete_shared_extension_profile() {
        let registry = mcp_extension_registry(BuiltinExtensionDependencies {
            auth_manager: AuthManager::from_auth_for_testing(CodexAuth::from_api_key("test")),
            state_db: None,
            analytics_events_client: None,
            thread_manager: Weak::new(),
            goal_service: Arc::new(GoalService::new()),
            environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
            session_source: SessionSource::Mcp,
        });

        assert_eq!(registry.tool_contributors().len(), 4);
        assert_eq!(registry.context_contributors().len(), 2);
        assert_eq!(
            registry
                .mcp_server_contributors()
                .iter()
                .map(|contributor| contributor.id())
                .collect::<Vec<_>>(),
            vec!["hosted_plugin_runtime", "selected_executor_plugin_mcp"]
        );
    }

    #[tokio::test]
    async fn tool_task_shutdown_drops_resources_held_by_in_flight_tasks() {
        let tool_tasks = ToolTasks::default();
        let (resource_tx, mut resource_rx) = tokio::sync::mpsc::channel::<()>(1);
        tool_tasks.spawn(async move {
            let _resource_tx = resource_tx;
            std::future::pending::<()>().await;
        });

        tokio::task::yield_now().await;
        tool_tasks.shutdown().await;

        assert_eq!(resource_rx.recv().await, None);
    }
}
