use codex_protocol::request_permissions::RequestPermissionsArgs;
use codex_sandboxing::policy_transforms::normalize_uri_additional_permissions;
use std::sync::Arc;

use crate::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::shell_spec::create_request_permissions_tool;
use crate::tools::handlers::shell_spec::request_permissions_tool_description;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutionTiming;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde_json::Value;

pub struct RequestPermissionsHandler;

#[derive(Deserialize)]
struct RequestPermissionsEnvironmentArgs {
    #[serde(default, rename = "environment_id", alias = "environmentId")]
    environment_id: Option<String>,
}

impl ToolExecutor<ToolInvocation> for RequestPermissionsHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("request_permissions")
    }

    fn spec(&self) -> ToolSpec {
        create_request_permissions_tool(request_permissions_tool_description())
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl RequestPermissionsHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            cancellation_token,
            call_id,
            payload,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "request_permissions handler received unsupported payload".to_string(),
                ));
            }
        };

        let environment_args: RequestPermissionsEnvironmentArgs = parse_arguments(&arguments)?;
        let Some(turn_environment) = resolve_tool_environment(
            &step_context.environments,
            environment_args.environment_id.as_deref(),
        )?
        else {
            return Err(FunctionCallError::RespondToModel(
                "request_permissions requires a primary environment".to_string(),
            ));
        };
        let mut args = parse_request_permissions_args(&arguments, turn_environment.cwd())?;
        args.permissions = normalize_uri_additional_permissions(args.permissions.into())
            .map(codex_protocol::request_permissions::RequestPermissionProfile::from)
            .map_err(FunctionCallError::RespondToModel)?;
        if args.permissions.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "request_permissions requires at least one permission".to_string(),
            ));
        }

        let response = session
            .request_permissions_for_environment(
                &turn,
                call_id,
                args,
                turn_environment.selection(),
                cancellation_token,
            )
            .await
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "request_permissions was cancelled before receiving a response".to_string(),
                )
            })?;

        let content = serde_json::to_string(&response).map_err(|err| {
            FunctionCallError::Fatal(format!(
                "failed to serialize request_permissions response: {err}"
            ))
        })?;

        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            content,
            Some(true),
        )))
    }
}

fn parse_request_permissions_args(
    arguments: &str,
    environment_cwd: &PathUri,
) -> Result<RequestPermissionsArgs, FunctionCallError> {
    let mut value: Value = parse_arguments(arguments)?;
    let file_system = value
        .get_mut("permissions")
        .and_then(|permissions| permissions.get_mut("file_system"));
    if let Some(file_system) = file_system {
        for field in ["read", "write"] {
            if let Some(paths) = file_system.get_mut(field).and_then(Value::as_array_mut) {
                for path in paths {
                    resolve_path_value(path, environment_cwd)?;
                }
            }
        }
        if let Some(entries) = file_system.get_mut("entries").and_then(Value::as_array_mut) {
            for entry in entries {
                if entry
                    .get("path")
                    .and_then(|path| path.get("type"))
                    .and_then(Value::as_str)
                    == Some("path")
                    && let Some(path) = entry.get_mut("path").and_then(|path| path.get_mut("path"))
                {
                    resolve_path_value(path, environment_cwd)?;
                }
            }
        }
    }
    serde_json::from_value(value).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
    })
}

fn resolve_path_value(
    value: &mut Value,
    environment_cwd: &PathUri,
) -> Result<(), FunctionCallError> {
    let Some(path) = value.as_str() else {
        return Err(FunctionCallError::RespondToModel(
            "request_permissions filesystem paths must be strings".to_string(),
        ));
    };
    let resolved = PathUri::parse(path).or_else(|_| environment_cwd.join(path));
    *value = Value::String(
        resolved
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to resolve permission path `{path}` against `{environment_cwd}`: {err}"
                ))
            })?
            .to_string(),
    );
    Ok(())
}

impl CoreToolRuntime for RequestPermissionsHandler {
    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        ToolExecutionTiming::Interactive
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::NetworkPermissions;
    use codex_protocol::permissions::FileSystemAccessMode;
    use codex_protocol::permissions::FileSystemPath;

    #[tokio::test]
    async fn permission_requests_wait_for_runtime_cancellation_cleanup() {
        use crate::session::step_context::StepContext;
        use crate::state::ActiveTurn;
        use crate::tools::ToolRouter;
        use crate::tools::parallel::ToolCallRuntime;
        use crate::tools::router::ToolCall;
        use crate::tools::router::ToolRouterParams;
        use crate::turn_diff_tracker::TurnDiffTracker;
        use codex_features::Feature;
        use codex_protocol::models::ResponseInputItem;
        use codex_protocol::protocol::AskForApproval;
        use codex_protocol::protocol::EventMsg;
        use codex_protocol::request_permissions::PermissionGrantScope;
        use codex_protocol::request_permissions::RequestPermissionProfile;
        use codex_protocol::request_permissions::RequestPermissionsResponse;
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        let (session, mut turn, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let context = Arc::get_mut(&mut turn).expect("unique turn context");
        context
            .approval_policy
            .set(AskForApproval::OnRequest)
            .expect("interactive approval policy");
        let _ = Arc::make_mut(&mut context.config)
            .features
            .enable(Feature::RequestPermissionsTool);
        let active = ActiveTurn::default();
        let turn_state = Arc::clone(&active.turn_state);
        *session.active_turn.lock().await = Some(active);
        let scope = turn
            .environments
            .primary()
            .expect("primary environment")
            .environment
            .approval_scope_id()
            .to_string();
        let mut held = session.subscribe_elicitation_pause_state();
        let step = StepContext::for_test(Arc::clone(&turn));
        let router = Arc::new(ToolRouter::from_context(
            step.as_ref(),
            ToolRouterParams {
                tool_suggest_candidates: None,
                deferred_mcp_tools: None,
                mcp_tools: None,
                extension_tool_executors: Vec::new(),
                dynamic_tools: turn.dynamic_tools.as_slice(),
                exposure_identity: Default::default(),
            },
            &Default::default(),
        ));
        let runtime = ToolCallRuntime::new(
            Arc::clone(&session),
            step.with_tool_router_for_test(router),
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        );
        let token = CancellationToken::new();
        let request = tokio::spawn(runtime.handle_tool_call(
            ToolCall {
                tool_name: ToolName::plain("request_permissions"),
                call_id: "runtime-cancelled-permission".to_string(),
                payload: ToolPayload::Function {
                    arguments: r#"{"permissions":{"network":{"enabled":true}}}"#.to_string(),
                },
            },
            token.clone(),
        ));
        let event = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let EventMsg::RequestPermissions(event) =
                    events.recv().await.expect("request event channel").msg
                {
                    break event;
                }
            }
        })
        .await
        .expect("registered permission request event");
        assert_eq!(event.call_id, "runtime-cancelled-permission");
        assert_eq!(
            event.permissions.network,
            Some(NetworkPermissions {
                enabled: Some(true)
            })
        );
        assert!(*held.borrow(), "live request holds interactive state");
        token.cancel();
        let response = tokio::time::timeout(Duration::from_secs(2), request)
            .await
            .expect("runtime cancellation completes")
            .expect("runtime task joins")
            .expect("runtime returns its terminal response");
        let ResponseInputItem::FunctionCallOutput { call_id, output } = response else {
            panic!("expected terminal permission function output");
        };
        assert_eq!(call_id, "runtime-cancelled-permission");
        assert_eq!(output.success, None);
        let aborted_text = output.body.to_text().expect("runtime cancellation text");
        let elapsed = aborted_text
            .strip_prefix("aborted by user after ")
            .and_then(|text| text.strip_suffix('s'))
            .expect("normal runtime returns its explicit aborted response")
            .parse::<f32>()
            .expect("aborted response includes elapsed seconds");
        assert!(elapsed.is_finite() && elapsed >= 0.0);
        tokio::time::timeout(Duration::from_secs(2), held.wait_for(|active| !*active))
            .await
            .expect("owned handler cleanup releases interactive state")
            .expect("elicitation service remains available");
        assert!(
            turn_state
                .lock()
                .await
                .remove_pending_request_permissions(&call_id)
                .is_none(),
            "cooperative runtime cancellation must remove its exact keyed waiter before returning"
        );
        session
            .notify_request_permissions_response(
                &call_id,
                RequestPermissionsResponse {
                    permissions: RequestPermissionProfile {
                        network: Some(NetworkPermissions {
                            enabled: Some(true),
                        }),
                        ..RequestPermissionProfile::default()
                    },
                    scope: PermissionGrantScope::Session,
                    strict_auto_review: false,
                },
            )
            .await;
        assert_eq!(session.granted_turn_permissions(&scope).await, None);
        assert_eq!(session.granted_session_permissions(&scope).await, None);
    }

    #[test]
    fn foreign_environment_accepts_network_only_permission_request() {
        let cwd = PathUri::parse("file:///home/remote/project").expect("foreign POSIX cwd");

        assert!(cwd.to_abs_path().is_err());

        let args =
            parse_request_permissions_args(r#"{"permissions":{"network":{"enabled":true}}}"#, &cwd)
                .expect("network-only request should not require host path conversion");

        assert_eq!(
            args.permissions.network,
            Some(NetworkPermissions {
                enabled: Some(true),
            })
        );
        assert!(args.permissions.file_system.is_none());
    }

    #[test]
    fn foreign_environment_resolves_file_system_permission_paths_as_uris() {
        let cwd = PathUri::parse("file:///home/remote/project").expect("foreign POSIX cwd");

        let args = parse_request_permissions_args(
            r#"{"permissions":{"file_system":{"entries":[{"path":{"type":"path","path":"generated/output.txt"},"access":"write"}]}}}"#,
            &cwd,
        )
        .expect("foreign filesystem request should retain URI paths");

        let file_system = args.permissions.file_system.expect("filesystem profile");
        assert!(matches!(
            file_system.entries.as_slice(),
            [codex_protocol::permissions::FileSystemSandboxEntry {
                path: FileSystemPath::Path { path },
                access: FileSystemAccessMode::Write,
            }] if path == &PathUri::parse("file:///home/remote/project/generated/output.txt").expect("expected URI")
        ));
    }
}
