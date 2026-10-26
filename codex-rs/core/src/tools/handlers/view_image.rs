use codex_protocol::items::ImageViewItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::InputModality;
use codex_utils_image::MAX_PROMPT_IMAGE_SOURCE_BYTES;
use codex_utils_image::data_url_from_bytes;
use serde::Deserialize;
use std::sync::Arc;

use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::view_image_spec::ViewImageToolOptions;
use crate::tools::handlers::view_image_spec::create_view_image_tool;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSearchSourceInfo;
use codex_tools::ToolSpec;
use codex_tools::can_request_original_image_detail;

pub struct ViewImageHandler {
    options: ViewImageToolOptions,
}

impl Default for ViewImageHandler {
    fn default() -> Self {
        Self {
            options: ViewImageToolOptions {
                can_request_original_image_detail: false,
                include_environment_id: false,
            },
        }
    }
}

impl ViewImageHandler {
    pub(crate) fn new(options: ViewImageToolOptions) -> Self {
        Self { options }
    }
}

const VIEW_IMAGE_UNSUPPORTED_MESSAGE: &str =
    "view_image is not allowed because you do not support image inputs";

#[derive(Deserialize)]
struct ViewImageArgs {
    path: String,
    #[serde(default)]
    environment_id: Option<String>,
    detail: Option<String>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ViewImageDetail {
    Low,
    High,
    Original,
}

impl ToolExecutor<ToolInvocation> for ViewImageHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("view_image")
    }

    fn spec(&self) -> ToolSpec {
        create_view_image_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn search_info_for_registered_spec(
        &self,
        registered_spec: &ToolSpec,
    ) -> Option<ToolSearchInfo> {
        ToolSearchInfo::from_tool_spec(
            registered_spec,
            Some(ToolSearchSourceInfo {
                name: "Images".to_string(),
                description: Some(
                    "Inspect a local image when visual context is needed.".to_string(),
                ),
            }),
        )
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl ViewImageHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        if !invocation
            .step_context
            .turn
            .model_info
            .input_modalities
            .contains(&InputModality::Image)
        {
            return Err(FunctionCallError::RespondToModel(
                VIEW_IMAGE_UNSUPPORTED_MESSAGE.to_string(),
            ));
        }

        let ToolInvocation {
            session,
            step_context,
            cancellation_token,
            payload,
            call_id,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "view_image handler received unsupported payload".to_string(),
                ));
            }
        };

        let ViewImageArgs {
            path,
            environment_id,
            detail,
        } = parse_arguments(&arguments)?;
        // `high` is the explicit spelling of the default resized path.
        // Other string values remain invalid rather than being silently reinterpreted.
        let detail = match detail.as_deref() {
            None => None,
            Some("low") => Some(ViewImageDetail::Low),
            Some("high") => Some(ViewImageDetail::High),
            Some("original") => Some(ViewImageDetail::Original),
            Some(detail) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "view_image.detail only supports `low`, `high` or `original`; omit `detail` for default high resized behavior, got `{detail}`"
                )));
            }
        };

        let Some(turn_environment) = wait_for_tool_environment(
            &step_context.environments,
            environment_id.as_deref(),
            &cancellation_token,
        )
        .await?
        else {
            return Err(FunctionCallError::RespondToModel(
                "view_image is unavailable in this session".to_string(),
            ));
        };
        let path_uri = turn_environment.cwd().join(&path).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "unable to resolve image path `{path}` against environment cwd `{}`: {err}",
                turn_environment.cwd(),
            ))
        })?;
        let model_visible_path = path_uri.inferred_native_path_string();
        let sandbox = turn.file_system_sandbox_context(
            /*additional_permissions*/ None,
            turn_environment.cwd(),
        );
        let fs = turn_environment.environment.get_filesystem();

        let metadata = fs
            .get_metadata(&path_uri, Some(&sandbox))
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "unable to locate image at `{model_visible_path}`: {error}"
                ))
            })?;

        if !metadata.is_file {
            return Err(FunctionCallError::RespondToModel(format!(
                "image path `{model_visible_path}` is not a file"
            )));
        }
        if metadata.size > MAX_PROMPT_IMAGE_SOURCE_BYTES as u64 {
            return Err(FunctionCallError::RespondToModel(format!(
                "image at `{model_visible_path}` is too large: {} bytes exceeds the view_image limit of {MAX_PROMPT_IMAGE_SOURCE_BYTES} bytes",
                metadata.size
            )));
        }
        let file_bytes = fs
            .read_file_bounded(&path_uri, MAX_PROMPT_IMAGE_SOURCE_BYTES, Some(&sandbox))
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "unable to read image at `{model_visible_path}`: {error}"
                ))
            })?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "unable to read image at `{model_visible_path}`: the file changed while being read or exceeds the view_image limit of {MAX_PROMPT_IMAGE_SOURCE_BYTES} bytes"
                ))
            })?;

        let can_request_original_detail = can_request_original_image_detail(&turn.model_info);
        let use_original_detail =
            can_request_original_detail && matches!(detail, Some(ViewImageDetail::Original));
        let image_detail = if use_original_detail {
            ImageDetail::Original
        } else if matches!(detail, Some(ViewImageDetail::Low)) {
            ImageDetail::Low
        } else {
            DEFAULT_IMAGE_DETAIL
        };

        // The history insertion path owns image decoding and resizing.
        let image_url = data_url_from_bytes("application/octet-stream", &file_bytes);

        let item = TurnItem::ImageView(ImageViewItem {
            environment: Some(turn_environment.provenance()),
            id: call_id,
            path: path_uri,
        });
        session.emit_turn_item_started(turn.as_ref(), &item).await;
        session.emit_turn_item_completed(turn.as_ref(), item).await;

        Ok(boxed_tool_output(ViewImageOutput {
            image_url,
            image_detail,
        }))
    }
}

impl CoreToolRuntime for ViewImageHandler {}

pub struct ViewImageOutput {
    image_url: String,
    image_detail: ImageDetail,
}

impl ToolOutput for ViewImageOutput {
    fn log_preview(&self) -> String {
        format!("<image data URL omitted: {} bytes>", self.image_url.len())
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        let body =
            FunctionCallOutputBody::ContentItems(vec![FunctionCallOutputContentItem::InputImage {
                image_url: self.image_url.clone(),
                detail: Some(self.image_detail),
            }]);
        let output = FunctionCallOutputPayload {
            body,
            success: Some(true),
        };

        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> serde_json::Value {
        serde_json::json!({
            "image_url": self.image_url,
            "detail": self.image_detail
        })
    }
}

