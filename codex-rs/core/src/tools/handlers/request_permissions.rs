use codex_protocol::request_permissions::RequestPermissionsArgs;
use codex_sandboxing::policy_transforms::normalize_additional_permissions;
use std::sync::Arc;

use crate::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::parse_arguments_with_base_path;
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
        args.permissions = normalize_additional_permissions(args.permissions.into())
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
    match environment_cwd.to_abs_path() {
        Ok(native_cwd) => parse_arguments_with_base_path(arguments, &native_cwd),
        Err(err) => {
            let args: RequestPermissionsArgs = parse_arguments(arguments)?;
            if args.permissions.file_system.is_some() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "request_permissions file-system grants require a cwd native to the Codex host; `{environment_cwd}` is foreign: {err}"
                )));
            }
            Ok(args)
        }
    }
}

impl CoreToolRuntime for RequestPermissionsHandler {
    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        ToolExecutionTiming::Interactive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::NetworkPermissions;

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
}
