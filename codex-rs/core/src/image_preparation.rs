use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ImageUrlKind;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::classify_image_url;
use codex_utils_image::ImageProcessingError;
use codex_utils_image::PromptImageMode;
use codex_utils_image::PromptImageResizeLimits;
use codex_utils_image::load_data_url_for_prompt;
use tracing::warn;

pub(crate) const IMAGE_PROCESSING_ERROR_PLACEHOLDER: &str =
    "image content omitted because it could not be processed";
const IMAGE_TOO_LARGE_PLACEHOLDER: &str =
    "image content omitted because it exceeded the supported size limit; use a smaller image";
const REMOTE_IMAGE_URL_PLACEHOLDER: &str =
    "image content omitted because remote image URLs are not supported";
const UNSUPPORTED_IMAGE_URL_PLACEHOLDER: &str =
    "image content omitted because only inline base64 data URLs are supported";

const LOW_DETAIL_LIMITS: PromptImageResizeLimits = PromptImageResizeLimits {
    max_dimension: 512,
    max_patches: 256,
};
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
    #[error("only inline base64 image data URLs are supported")]
    RepresentationUnsupported,
    #[error(transparent)]
    Processing(#[from] ImageProcessingError),
}

impl ImagePreparationError {
    fn placeholder(&self) -> &'static str {
        match self {
            ImagePreparationError::RemoteUrlUnsupported => REMOTE_IMAGE_URL_PLACEHOLDER,
            ImagePreparationError::RepresentationUnsupported => UNSUPPORTED_IMAGE_URL_PLACEHOLDER,
            ImagePreparationError::Processing(ImageProcessingError::ImageTooLarge { .. }) => {
                IMAGE_TOO_LARGE_PLACEHOLDER
            }
            ImagePreparationError::Processing(_) => IMAGE_PROCESSING_ERROR_PLACEHOLDER,
        }
    }
}

pub(crate) fn prepare_response_items(items: &mut [ResponseItem]) {
    prepare_response_items_with_detail_policy(items, true);
}

// This projection runs after fallback selection; canonical history and its
// cache never acquire model-specific detail or resizing decisions.
pub(crate) fn for_model_image_detail(
    mut items: std::sync::Arc<[ResponseItem]>,
    supports_original: bool,
) -> std::sync::Arc<[ResponseItem]> {
    let has_original = items.iter().any(|item| match item {
        ResponseItem::Message { content, .. } => content.iter().any(|item| {
            matches!(
                item,
                ContentItem::InputImage {
                    detail: Some(ImageDetail::Original),
                    ..
                }
            )
        }),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().is_some_and(|content| {
                content.iter().any(|item| {
                    matches!(
                        item,
                        FunctionCallOutputContentItem::InputImage {
                            detail: Some(ImageDetail::Original),
                            ..
                        }
                    )
                })
            })
        }
        _ => false,
    });
    if !supports_original && has_original {
        prepare_response_items_with_detail_policy(std::sync::Arc::make_mut(&mut items), false);
    }
    items
}

fn prepare_response_items_with_detail_policy(items: &mut [ResponseItem], supports_original: bool) {
    for item in items {
        match item {
            ResponseItem::Message { content, .. } => {
                prepare_message_content(content, supports_original)
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content) = output.content_items_mut() {
                    prepare_tool_output_content(content, supports_original);
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
        ResponseItem::Message { content, .. } => content
            .iter()
            .any(|item| matches!(item, ContentItem::InputImage { .. })),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().is_some_and(|content| {
                content
                    .iter()
                    .any(|item| matches!(item, FunctionCallOutputContentItem::InputImage { .. }))
            })
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
        | ResponseItem::Other => false,
    })
}

fn prepare_message_content(items: &mut Vec<ContentItem>, supports_original: bool) {
    let mut index = 0;
    while index < items.len() {
        if let ContentItem::InputImage { image_url, detail } = &mut items[index] {
            if !supports_original && *detail != Some(ImageDetail::Original) {
                index += 1;
                continue;
            }
            let downgraded = !supports_original;
            if downgraded {
                *detail = Some(ImageDetail::High);
            }
            match prepare_image(image_url, *detail).map(|notice| detail_notice(notice, downgraded))
            {
                Ok(Some(text)) => {
                    let notice_index = if matches!(items.get(index + 1), Some(ContentItem::InputText { text }) if codex_protocol::models::is_image_close_tag_text(text))
                    {
                        index + 2
                    } else {
                        index + 1
                    };
                    if !matches!(items.get(notice_index), Some(ContentItem::InputText { text: existing }) if existing == &text)
                    {
                        items.insert(notice_index, ContentItem::InputText { text });
                    }
                    index += 1;
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(%error, "failed to prepare message image");
                    items[index] = ContentItem::InputText {
                        text: error.placeholder().to_string(),
                    };
                }
            }
        }
        index += 1;
    }
}

fn prepare_tool_output_content(
    items: &mut Vec<FunctionCallOutputContentItem>,
    supports_original: bool,
) {
    let mut index = 0;
    while index < items.len() {
        if let FunctionCallOutputContentItem::InputImage { image_url, detail } = &mut items[index] {
            if !supports_original && *detail != Some(ImageDetail::Original) {
                index += 1;
                continue;
            }
            let downgraded = !supports_original;
            if downgraded {
                *detail = Some(ImageDetail::High);
            }
            match prepare_image(image_url, *detail).map(|notice| detail_notice(notice, downgraded))
            {
                Ok(Some(text)) => {
                    if !matches!(items.get(index + 1), Some(FunctionCallOutputContentItem::InputText { text: existing }) if existing == &text)
                    {
                        items.insert(index + 1, FunctionCallOutputContentItem::InputText { text });
                    }
                    index += 1;
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(%error, "failed to prepare tool output image");
                    items[index] = FunctionCallOutputContentItem::InputText {
                        text: error.placeholder().to_string(),
                    };
                }
            }
        }
        index += 1;
    }
}

fn detail_notice(notice: Option<String>, downgraded: bool) -> Option<String> {
    let notice = if !downgraded {
        notice
    } else {
        Some(format!(
            "Original image detail is unavailable for this model; high-detail resolution limits apply.{}",
            notice.map_or_else(String::new, |notice| format!(" {notice}")),
        ))
    };
    notice.map(|notice| format!("<image_preparation_notice>{notice}</image_preparation_notice>"))
}

pub(crate) fn is_image_preparation_notice(text: &str) -> bool {
    text.starts_with("<image_preparation_notice>") && text.ends_with("</image_preparation_notice>")
}

fn prepare_image(
    image_url: &mut String,
    detail: Option<ImageDetail>,
) -> Result<Option<String>, ImagePreparationError> {
    match classify_image_url(image_url) {
        ImageUrlKind::InlineData => {}
        ImageUrlKind::Remote => return Err(ImagePreparationError::RemoteUrlUnsupported),
        ImageUrlKind::Unsupported => return Err(ImagePreparationError::RepresentationUnsupported),
    }

    let limits = match detail {
        None | Some(ImageDetail::Auto | ImageDetail::High) => HIGH_DETAIL_LIMITS,
        Some(ImageDetail::Original) => ORIGINAL_DETAIL_LIMITS,
        Some(ImageDetail::Low) => LOW_DETAIL_LIMITS,
    };
    let image = load_data_url_for_prompt(image_url, PromptImageMode::ResizeWithLimits(limits))?;
    let mut notice = String::new();
    if detail == Some(ImageDetail::Original)
        && image.source_dimensions != (image.width, image.height)
    {
        notice = format!(
            "Original-detail image resized from {}x{} to {}x{} to satisfy image safety limits; exact source resolution was not delivered.",
            image.source_dimensions.0, image.source_dimensions.1, image.width, image.height,
        );
    }
    if image.animation_present {
        if !notice.is_empty() {
            notice.push(' ');
        }
        notice.push_str("This image contains animation. Only a static first/default image is inspected; other animation frames have not been analyzed.");
    }
    *image_url = image.into_data_url();
    Ok((!notice.is_empty()).then_some(notice))
}

#[cfg(test)]
#[path = "image_preparation_tests.rs"]
mod tests;
