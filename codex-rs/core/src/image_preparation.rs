use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use codex_utils_image::ImageProcessingError;
use codex_utils_image::PromptImageMode;
use codex_utils_image::PromptImageResizeLimits;
use codex_utils_image::load_data_url_for_prompt;
use tracing::warn;

pub(crate) const IMAGE_PROCESSING_ERROR_PLACEHOLDER: &str =
    "image content omitted because it could not be processed";
const IMAGE_TOO_LARGE_PLACEHOLDER: &str =
    "image content omitted because it exceeded the supported size limit; use a smaller image";
const UNSUPPORTED_LOW_DETAIL_PLACEHOLDER: &str = "image content omitted because detail 'low' is not supported; use 'high', 'original', or 'auto'";
const REMOTE_IMAGE_URL_PLACEHOLDER: &str =
    "image content omitted because remote image URLs are not supported";

const HIGH_DETAIL_LIMITS: PromptImageResizeLimits = PromptImageResizeLimits {
    max_dimension: 2048,
    max_patches: 2_500,
};
const ORIGINAL_DETAIL_LIMITS: PromptImageResizeLimits = PromptImageResizeLimits {
    max_dimension: 6000,
    max_patches: 10_000,
};
#[derive(Debug, thiserror::Error)]
enum ImagePreparationError {
    #[error("remote image URLs are not supported")]
    RemoteUrlUnsupported,
    #[error("image detail `low` is not supported")]
    UnsupportedLowDetail,
    #[error(transparent)]
    Processing(#[from] ImageProcessingError),
}

impl ImagePreparationError {
    fn placeholder(&self) -> &'static str {
        match self {
            ImagePreparationError::RemoteUrlUnsupported => REMOTE_IMAGE_URL_PLACEHOLDER,
            ImagePreparationError::UnsupportedLowDetail => UNSUPPORTED_LOW_DETAIL_PLACEHOLDER,
            ImagePreparationError::Processing(ImageProcessingError::ImageTooLarge { .. }) => {
                IMAGE_TOO_LARGE_PLACEHOLDER
            }
            ImagePreparationError::Processing(_) => IMAGE_PROCESSING_ERROR_PLACEHOLDER,
        }
    }
}

pub(crate) fn prepare_response_items(items: &mut [ResponseItem]) {
    for item in items {
        match item {
            ResponseItem::Message { content, .. } => prepare_message_content(content),
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content) = output.content_items_mut() {
                    prepare_tool_output_content(content);
                }
            }
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => {}
        }
    }
}

pub(crate) fn response_items_need_preparation(items: &[ResponseItem]) -> bool {
    items.iter().any(|item| match item {
        ResponseItem::Message { content, .. } => content.iter().any(|item| match item {
            ContentItem::InputImage { image_url, .. } => {
                is_remote_image_url(image_url) || is_data_url(image_url)
            }
            ContentItem::InputText { .. } | ContentItem::OutputText { .. } => false,
        }),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().is_some_and(|content| {
                content.iter().any(|item| match item {
                    FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                        is_remote_image_url(image_url) || is_data_url(image_url)
                    }
                    FunctionCallOutputContentItem::InputText { .. }
                    | FunctionCallOutputContentItem::EncryptedContent { .. } => false,
                })
            })
        }
        _ => false,
    })
}

/// Prepares an owned vector off the async executor and publishes only the complete result.
pub(crate) async fn prepare_response_items_owned(
    items: Vec<ResponseItem>,
) -> Result<Vec<ResponseItem>, tokio::task::JoinError> {
    if !response_items_need_preparation(&items) {
        return Ok(items);
    }
    prepare_response_items_owned_with(items, prepare_response_items).await
}

async fn prepare_response_items_owned_with(
    mut items: Vec<ResponseItem>,
    prepare: impl FnOnce(&mut [ResponseItem]) + Send + 'static,
) -> Result<Vec<ResponseItem>, tokio::task::JoinError> {
    await_response_items_preparation(tokio::task::spawn_blocking(move || {
        prepare(&mut items);
        items
    }))
    .await
}

async fn await_response_items_preparation(
    task: tokio::task::JoinHandle<Vec<ResponseItem>>,
) -> Result<Vec<ResponseItem>, tokio::task::JoinError> {
    match task.await {
        Ok(items) => Ok(items),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => {
            warn!(%error, "image preparation task was cancelled");
            Err(error)
        }
    }
}

fn prepare_message_content(items: &mut [ContentItem]) {
    for item in items {
        if let ContentItem::InputImage { image_url, detail } = item
            && let Err(error) = prepare_image(image_url, *detail)
        {
            warn!(%error, "failed to prepare message image");
            *item = ContentItem::InputText {
                text: error.placeholder().to_string(),
            };
        }
    }
}

fn prepare_tool_output_content(items: &mut [FunctionCallOutputContentItem]) {
    for item in items {
        if let FunctionCallOutputContentItem::InputImage { image_url, detail } = item
            && let Err(error) = prepare_image(image_url, *detail)
        {
            warn!(%error, "failed to prepare tool output image");
            *item = FunctionCallOutputContentItem::InputText {
                text: error.placeholder().to_string(),
            };
        }
    }
}

fn is_remote_image_url(image_url: &str) -> bool {
    image_url.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}

fn is_data_url(image_url: &str) -> bool {
    image_url
        .get(.."data:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
}

fn prepare_image(
    image_url: &mut String,
    detail: Option<ImageDetail>,
) -> Result<(), ImagePreparationError> {
    if is_remote_image_url(image_url) {
        return Err(ImagePreparationError::RemoteUrlUnsupported);
    }
    if !is_data_url(image_url) {
        return Ok(());
    }

    let limits = match detail {
        None | Some(ImageDetail::Auto | ImageDetail::High) => HIGH_DETAIL_LIMITS,
        Some(ImageDetail::Original) => ORIGINAL_DETAIL_LIMITS,
        Some(ImageDetail::Low) => return Err(ImagePreparationError::UnsupportedLowDetail),
    };
    let image = load_data_url_for_prompt(image_url, PromptImageMode::ResizeWithLimits(limits))?;
    *image_url = image.into_data_url();
    Ok(())
}

#[cfg(test)]
#[path = "image_preparation_tests.rs"]
mod tests;
