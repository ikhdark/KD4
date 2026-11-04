//! Project legacy tool events into history items without owning replay orchestration.

use super::ThreadHistoryBuilder;
use codex_app_server_protocol_types::DynamicToolCallOutputContentItem;
use codex_app_server_protocol_types::DynamicToolCallStatus;
use codex_app_server_protocol_types::McpToolCallAppContext;
use codex_app_server_protocol_types::McpToolCallError;
use codex_app_server_protocol_types::McpToolCallResult;
use codex_app_server_protocol_types::McpToolCallStatus;
use codex_app_server_protocol_types::ThreadItem;
use codex_app_server_protocol_types::WebSearchItem;
use codex_app_server_protocol_types::build_command_execution_begin_item;
use codex_app_server_protocol_types::build_command_execution_end_item;
use codex_app_server_protocol_types::build_file_change_approval_request_item;
use codex_app_server_protocol_types::build_file_change_begin_item;
use codex_app_server_protocol_types::build_file_change_end_item;
use codex_app_server_protocol_types::web_search_action_from_core;
use codex_extension_items::image_generation::ImageGenerationItem;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::DynamicToolCallResponseEvent;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::ImageGenerationBeginEvent;
use codex_protocol::protocol::ImageGenerationEndEvent;
use codex_protocol::protocol::McpToolCallBeginEvent;
use codex_protocol::protocol::McpToolCallEndEvent;
use codex_protocol::protocol::PatchApplyBeginEvent;
use codex_protocol::protocol::PatchApplyEndEvent;
use codex_protocol::protocol::ViewImageToolCallEvent;
use codex_protocol::protocol::WebSearchBeginEvent;
use codex_protocol::protocol::WebSearchEndEvent;

impl ThreadHistoryBuilder {
    pub(super) fn handle_web_search_begin(&mut self, payload: &WebSearchBeginEvent) {
        if self.current_turn_has_item(&payload.call_id) {
            return;
        }
        let item = ThreadItem::WebSearch(WebSearchItem {
            status: None,
            error: None,
            id: payload.call_id.clone(),
            query: String::new(),
            action: None,
        });
        self.upsert_item_in_current_turn(item);
    }

    pub(super) fn handle_web_search_end(&mut self, payload: &WebSearchEndEvent) {
        let item = ThreadItem::WebSearch(WebSearchItem {
            status: None,
            error: None,
            id: payload.call_id.clone(),
            query: payload.query.clone(),
            action: Some(web_search_action_from_core(payload.action.clone())),
        });
        self.upsert_legacy_tool_end(item);
    }

    pub(super) fn handle_exec_command_begin(&mut self, payload: &ExecCommandBeginEvent) {
        if self.omit_tool_payloads {
            self.upsert_item_in_turn_id(
                &payload.turn_id,
                Self::omitted_tool_item(payload.call_id.clone()),
            );
            return;
        }
        let item = build_command_execution_begin_item(payload);
        self.upsert_item_in_turn_id(&payload.turn_id, item);
    }

    pub(super) fn handle_exec_command_end(&mut self, payload: &ExecCommandEndEvent) {
        if self.omit_tool_payloads {
            self.upsert_item_in_turn_id(
                &payload.turn_id,
                Self::omitted_tool_item(payload.call_id.clone()),
            );
            return;
        }
        let item = build_command_execution_end_item(payload);
        // Command completions can arrive out of order. Unified exec may return
        // while a PTY is still running, then emit ExecCommandEnd later from a
        // background exit watcher when that process finally exits. By then, a
        // newer user turn may already have started. Route by event turn_id so
        // replay preserves the original turn association.
        self.upsert_item_in_turn_id(&payload.turn_id, item);
    }

    pub(super) fn handle_apply_patch_approval_request(
        &mut self,
        payload: &ApplyPatchApprovalRequestEvent,
    ) {
        let item = if self.omit_tool_payloads {
            Self::omitted_tool_item(payload.call_id.clone())
        } else {
            build_file_change_approval_request_item(payload)
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    pub(super) fn handle_patch_apply_begin(&mut self, payload: &PatchApplyBeginEvent) {
        let item = if self.omit_tool_payloads {
            Self::omitted_tool_item(payload.call_id.clone())
        } else {
            build_file_change_begin_item(payload)
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    pub(super) fn handle_patch_apply_end(&mut self, payload: &PatchApplyEndEvent) {
        let item = if self.omit_tool_payloads {
            Self::omitted_tool_item(payload.call_id.clone())
        } else {
            build_file_change_end_item(payload)
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    pub(super) fn handle_dynamic_tool_call_request(
        &mut self,
        payload: &codex_protocol::dynamic_tools::DynamicToolCallRequest,
    ) {
        if self.omit_tool_payloads {
            let item = Self::omitted_tool_item(payload.call_id.clone());
            if payload.turn_id.is_empty() {
                self.upsert_item_in_current_turn(item);
            } else {
                self.upsert_item_in_turn_id(&payload.turn_id, item);
            }
            return;
        }
        let item = ThreadItem::DynamicToolCall {
            id: payload.call_id.clone(),
            namespace: payload.namespace.clone(),
            tool: payload.tool.clone(),
            arguments: payload.arguments.clone(),
            status: DynamicToolCallStatus::InProgress,
            content_items: None,
            success: None,
            error: None,
            duration_ms: None,
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    pub(super) fn handle_dynamic_tool_call_response(
        &mut self,
        payload: &DynamicToolCallResponseEvent,
    ) {
        if self.omit_tool_payloads {
            let item = Self::omitted_tool_item(payload.call_id.clone());
            if payload.turn_id.is_empty() {
                self.upsert_item_in_current_turn(item);
            } else {
                self.upsert_item_in_turn_id(&payload.turn_id, item);
            }
            return;
        }
        let status = if payload.success {
            DynamicToolCallStatus::Completed
        } else {
            DynamicToolCallStatus::Failed
        };
        let duration_ms = i64::try_from(payload.duration.as_millis()).ok();
        let item = ThreadItem::DynamicToolCall {
            id: payload.call_id.clone(),
            namespace: payload.namespace.clone(),
            tool: payload.tool.clone(),
            arguments: payload.arguments.clone(),
            status,
            content_items: Some(convert_dynamic_tool_content_items(&payload.content_items)),
            success: Some(payload.success),
            error: payload.error.clone(),
            duration_ms,
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    pub(super) fn handle_mcp_tool_call_begin(&mut self, payload: &McpToolCallBeginEvent) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_tool_item(payload.call_id.clone()));
            return;
        }
        let item = ThreadItem::McpToolCall {
            id: payload.call_id.clone(),
            server: payload.invocation.server.clone(),
            tool: payload.invocation.tool.clone(),
            status: McpToolCallStatus::InProgress,
            arguments: payload
                .invocation
                .arguments
                .clone()
                .unwrap_or(serde_json::Value::Null),
            app_context: payload
                .connector_id
                .clone()
                .map(|connector_id| McpToolCallAppContext {
                    connector_id,
                    link_id: payload.link_id.clone(),
                    resource_uri: payload.mcp_app_resource_uri.clone(),
                    app_name: payload.app_name.clone(),
                    template_id: payload.template_id.clone(),
                    action_name: payload.action_name.clone(),
                }),
            mcp_app_resource_uri: payload.mcp_app_resource_uri.clone(),
            plugin_id: payload.plugin_id.clone(),
            result: None,
            error: None,
            duration_ms: None,
        };
        self.upsert_item_in_current_turn(item);
    }

    pub(super) fn handle_mcp_tool_call_end(&mut self, payload: &McpToolCallEndEvent) {
        if self.omit_tool_payloads {
            self.upsert_legacy_tool_end(Self::omitted_tool_item(payload.call_id.clone()));
            return;
        }
        let status = if payload.is_success() {
            McpToolCallStatus::Completed
        } else {
            McpToolCallStatus::Failed
        };
        let duration_ms = i64::try_from(payload.duration.as_millis()).ok();
        let (result, error) = match &payload.result {
            Ok(value) => (
                Some(Box::new(McpToolCallResult {
                    content: value.content.clone(),
                    structured_content: value.structured_content.clone(),
                    is_error: value.is_error,
                    meta: value.meta.clone(),
                })),
                None,
            ),
            Err(message) => (
                None,
                Some(McpToolCallError {
                    message: message.clone(),
                }),
            ),
        };
        let item = ThreadItem::McpToolCall {
            id: payload.call_id.clone(),
            server: payload.invocation.server.clone(),
            tool: payload.invocation.tool.clone(),
            status,
            arguments: payload
                .invocation
                .arguments
                .clone()
                .unwrap_or(serde_json::Value::Null),
            app_context: payload
                .connector_id
                .clone()
                .map(|connector_id| McpToolCallAppContext {
                    connector_id,
                    link_id: payload.link_id.clone(),
                    resource_uri: payload.mcp_app_resource_uri.clone(),
                    app_name: payload.app_name.clone(),
                    template_id: payload.template_id.clone(),
                    action_name: payload.action_name.clone(),
                }),
            mcp_app_resource_uri: payload.mcp_app_resource_uri.clone(),
            plugin_id: payload.plugin_id.clone(),
            result,
            error,
            duration_ms,
        };
        self.upsert_legacy_tool_end(item);
    }

    pub(super) fn handle_view_image_tool_call(&mut self, payload: &ViewImageToolCallEvent) {
        let item = ThreadItem::ImageView {
            environment: payload.environment.clone(),
            id: payload.call_id.clone(),
            path: payload.path.clone().into(),
        };
        self.upsert_item_in_current_turn(item);
    }

    pub(super) fn handle_image_generation_begin(&mut self, payload: &ImageGenerationBeginEvent) {
        if self.current_turn_has_item(&payload.call_id) {
            return;
        }
        let item = ThreadItem::ImageGeneration(ImageGenerationItem {
            id: payload.call_id.clone(),
            status: String::new(),
            revised_prompt: None,
            result: None,
            error: None,
            saved_path: None,
            provider_usage: None,
        });
        self.upsert_item_in_current_turn(item);
    }

    pub(super) fn handle_image_generation_end(&mut self, payload: &ImageGenerationEndEvent) {
        if self.omit_tool_payloads {
            self.upsert_legacy_tool_end(Self::omitted_tool_item(payload.call_id.clone()));
            return;
        }
        // Legacy ends cannot express usage. Keep the same turn/call's receipt,
        // without copying or comparing image bytes or suppressing later updates.
        let provider_usage = self.current_turn.as_ref().and_then(|turn| {
            let index = *turn.item_indexes.get(&payload.call_id)?;
            match &turn.items[index] {
                ThreadItem::ImageGeneration(item) => item.provider_usage.clone(),
                _ => None,
            }
        });
        let item = ThreadItem::ImageGeneration(ImageGenerationItem {
            id: payload.call_id.clone(),
            status: payload.status.clone(),
            revised_prompt: payload.revised_prompt.clone(),
            result: payload.result.clone(),
            error: payload.error.clone(),
            saved_path: payload.saved_path.clone(),
            provider_usage,
        });
        self.upsert_legacy_tool_end(item);
    }
}

fn convert_dynamic_tool_content_items(
    items: &[codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem],
) -> Vec<DynamicToolCallOutputContentItem> {
    items
        .iter()
        .cloned()
        .map(DynamicToolCallOutputContentItem::from)
        .collect()
}
