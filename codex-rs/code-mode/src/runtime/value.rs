use serde_json::Value as JsonValue;

use codex_code_mode_protocol::DEFAULT_IMAGE_DETAIL;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::ImageDetail;

const IMAGE_HELPER_EXPECTS_MESSAGE: &str = "image expects a non-empty image URL string, an object with image_url and optional detail, or a raw MCP image block";
const REMOTE_IMAGE_URL_ERROR: &str = "Tool call failed: remote image URLs are not supported in tool outputs. Pass a base64 data URI instead";
const INVALID_IMAGE_URL_ERROR: &str =
    "Tool call failed: invalid image output. Pass a base64 data URI instead";
const CODEX_IMAGE_DETAIL_META_KEY: &str = "codex/imageDetail";

// These bound Rust conversion copies, not V8 allocations or buffered output.
pub(super) const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_NOTIFICATION_BYTES: usize = 1024 * 1024;

pub(super) fn bounded_string(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::String>,
    max_bytes: usize,
) -> Result<String, String> {
    if value.utf8_length(scope) > max_bytes {
        return Err(format!("payload exceeds its limit of {max_bytes} bytes"));
    }
    Ok(copy_string(scope, value))
}

fn copy_string(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<'_, v8::String>) -> String {
    #[cfg(test)]
    {
        let bytes = value.utf8_length(scope);
        if let Some(state) = scope.get_slot_mut::<super::RuntimeState>() {
            state.rust_conversion_bytes += bytes;
        }
    }
    value.to_rust_string_lossy(scope)
}

pub(super) fn serialize_output_text(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<String, String> {
    serialize_output_text_with_limit(scope, value, MAX_PAYLOAD_BYTES)
}

pub(super) fn serialize_output_text_with_limit(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    max_bytes: usize,
) -> Result<String, String> {
    if value.is_undefined()
        || value.is_null()
        || value.is_boolean()
        || value.is_number()
        || value.is_big_int()
        || value.is_string()
    {
        let value = value.to_string(scope).ok_or_else(|| "failed to format text".to_string())?;
        return bounded_string(scope, value, max_bytes);
    }

    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    if let Some(stringified) = v8::json::stringify(&tc, value) {
        return bounded_string(&mut tc, stringified, max_bytes);
    }
    if tc.has_caught() {
        return Err(tc
            .exception()
            .map(|exception| value_to_error_text(&mut tc, exception))
            .unwrap_or_else(|| "unknown code mode exception".to_string()));
    }
    let value = value.to_string(&tc).ok_or_else(|| "failed to format text".to_string())?;
    bounded_string(&mut tc, value, max_bytes)
}

pub(super) fn serialize_console_text(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    max_bytes: usize,
) -> Result<String, String> {
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    if value.is_native_error()
        && let Ok(object) = v8::Local::<v8::Object>::try_from(value)
        && let Some(key) = v8::String::new(&tc, "stack")
        && let Some(stack) = object.get(&tc, key.into())
        && let Ok(stack) = v8::Local::<v8::String>::try_from(stack)
    {
        return bounded_string(&mut tc, stack, max_bytes);
    }
    tc.reset();
    if value.is_object() {
        if let Some(stringified) = v8::json::stringify(&tc, value) {
            return bounded_string(&mut tc, stringified, max_bytes);
        }
        if tc.has_caught() && max_bytes >= 24 {
            tc.reset();
            return Ok("[unserializable object]".to_string());
        }
    }
    serialize_output_text_with_limit(&mut tc, value, max_bytes)
}

pub(super) fn normalize_output_image(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    detail_override: Option<String>,
) -> Result<FunctionCallOutputContentItem, ()> {
    let result = (|| -> Result<FunctionCallOutputContentItem, String> {
        let (image_url, detail) = if value.is_string() {
            (value.to_rust_string_lossy(scope), None)
        } else if value.is_object() && !value.is_array() {
            let object = v8::Local::<v8::Object>::try_from(value)
                .map_err(|_| IMAGE_HELPER_EXPECTS_MESSAGE.to_string())?;
            if let Some(image) = parse_non_mcp_output_image(scope, object)? {
                image
            } else {
                parse_mcp_output_image(scope, value)?
            }
        } else {
            return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
        };

        if image_url.is_empty() {
            return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
        }
        let Some((scheme, _)) = image_url.split_once(':') else {
            return Err(INVALID_IMAGE_URL_ERROR.to_string());
        };
        if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            return Err(REMOTE_IMAGE_URL_ERROR.to_string());
        }
        if !scheme.eq_ignore_ascii_case("data") || !valid_image_data_uri(&image_url) {
            return Err(INVALID_IMAGE_URL_ERROR.to_string());
        }

        let detail = detail_override.or(detail);
        let detail = match detail {
            Some(detail) => {
                let normalized = detail.to_ascii_lowercase();
                Some(match normalized.as_str() {
                    "auto" => ImageDetail::Auto,
                    "low" => ImageDetail::Low,
                    "high" => ImageDetail::High,
                    "original" => ImageDetail::Original,
                    _ => {
                        return Err(
                            "image detail must be one of: auto, low, high, original".to_string()
                        );
                    }
                })
            }
            None => Some(DEFAULT_IMAGE_DETAIL),
        };

        Ok(FunctionCallOutputContentItem::InputImage { image_url, detail })
    })();

    match result {
        Ok(item) => Ok(item),
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            Err(())
        }
    }
}

fn parse_non_mcp_output_image(
    scope: &mut v8::PinScope<'_, '_>,
    object: v8::Local<'_, v8::Object>,
) -> Result<Option<(String, Option<String>)>, String> {
    let image_url_key = v8::String::new(scope, "image_url")
        .ok_or_else(|| "failed to allocate image helper keys".to_string())?;
    let Some(image_url) = object.get(scope, image_url_key.into()) else {
        return Ok(None);
    };
    if image_url.is_undefined() {
        return Ok(None);
    }
    if !image_url.is_string() {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
    }
    let detail_key = v8::String::new(scope, "detail")
        .ok_or_else(|| "failed to allocate image helper keys".to_string())?;
    let detail = parse_image_detail_value(scope, object.get(scope, detail_key.into()))?;
    Ok(Some((image_url.to_rust_string_lossy(scope), detail)))
}

fn parse_mcp_output_image(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<(String, Option<String>), String> {
    let Some(result) = v8_value_to_json(scope, value)? else {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
    };
    let JsonValue::Object(result) = result else {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
    };
    let Some(item_type) = result.get("type").and_then(JsonValue::as_str) else {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
    };
    if item_type != "image" {
        return Err(format!(
            "image only accepts MCP image blocks, got \"{item_type}\""
        ));
    }
    let data = result
        .get("data")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "image expected MCP image data".to_string())?;
    if data.is_empty() {
        return Err("image expected MCP image data".to_string());
    }

    let image_url = if data.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:")) {
        data.to_string()
    } else {
        let mime_type = result
            .get("mimeType")
            .or_else(|| result.get("mime_type"))
            .and_then(JsonValue::as_str)
            .filter(|mime_type| !mime_type.is_empty())
            .unwrap_or("application/octet-stream");
        format!("data:{mime_type};base64,{data}")
    };
    let detail = result
        .get("_meta")
        .and_then(JsonValue::as_object)
        .and_then(|meta| meta.get(CODEX_IMAGE_DETAIL_META_KEY))
        .and_then(JsonValue::as_str)
        .filter(|detail| matches!(*detail, "auto" | "low" | "high" | "original"))
        .map(str::to_string);
    Ok((image_url, detail))
}

fn parse_image_detail_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: Option<v8::Local<'s, v8::Value>>,
) -> Result<Option<String>, String> {
    match value {
        Some(value) if value.is_string() => Ok(Some(value.to_rust_string_lossy(scope))),
        Some(value) if value.is_null() || value.is_undefined() => Ok(None),
        Some(_) => Err("image detail must be a string when provided".to_string()),
        None => Ok(None),
    }
}

pub(super) fn v8_value_to_json(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<Option<JsonValue>, String> {
    v8_value_to_json_with_limit(scope, value, MAX_PAYLOAD_BYTES).map_err(|error| match error {
        JsonConversionError::TooLarge => format!("payload exceeds its limit of {MAX_PAYLOAD_BYTES} bytes"),
        JsonConversionError::Invalid(message) => message,
    })
}

pub(super) enum JsonConversionError {
    TooLarge,
    Invalid(String),
}

pub(super) fn v8_value_to_json_with_limit(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    max_bytes: usize,
) -> Result<Option<JsonValue>, JsonConversionError> {
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let Some(stringified) = v8::json::stringify(&tc, value) else {
        if tc.has_caught() {
            return Err(JsonConversionError::Invalid(tc
                .exception()
                .map(|exception| value_to_error_text(&mut tc, exception))
                .unwrap_or_else(|| "unknown code mode exception".to_string())));
        }
        return Ok(None);
    };
    if stringified.utf8_length(&tc) > max_bytes {
        return Err(JsonConversionError::TooLarge);
    }
    serde_json::from_str(&copy_string(&mut tc, stringified))
        .map(Some)
        .map_err(|err| JsonConversionError::Invalid(format!("failed to serialize JavaScript value: {err}")))
}

pub(super) fn json_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &JsonValue,
) -> Option<v8::Local<'s, v8::Value>> {
    let json = serde_json::to_string(value).ok()?;
    let json = v8::String::new(scope, &json)?;
    v8::json::parse(scope, json)
}

pub(super) fn value_to_error_text(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> String {
    if value.is_object()
        && let Ok(object) = v8::Local::<v8::Object>::try_from(value)
        && let Some(key) = v8::String::new(scope, "stack")
        && let Some(stack) = object.get(scope, key.into())
        && stack.is_string()
    {
        return stack.to_rust_string_lossy(scope);
    }
    value.to_rust_string_lossy(scope)
}

pub(super) fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    if let Some(message) = v8::String::new(scope, message) {
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
    }
}

pub(super) fn error_value<'s>(scope: &mut v8::PinScope<'s, '_>, message: &str) -> v8::Local<'s, v8::Value> {
    match v8::String::new(scope, message) {
        Some(message) => v8::Exception::error(scope, message),
        None => v8::undefined(scope).into(),
    }
}

fn valid_image_data_uri(uri: &str) -> bool {
    let Some((header, payload)) = uri.split_once(',') else { return false; };
    let Some((mime, encoding)) = header.get(5..).and_then(|header| header.split_once(';')) else { return false; };
    // view_image and MCP blocks without a MIME type return opaque source bytes.
    // The history insertion path decodes and validates those bytes as an image.
    if !["image/png", "image/jpeg", "image/webp", "image/gif", "application/octet-stream"].iter().any(|supported| mime.eq_ignore_ascii_case(supported))
        || !encoding.eq_ignore_ascii_case("base64") || payload.is_empty() || payload.len() % 4 != 0 {
        return false;
    }
    // Validate canonical base64 in place; decoding would allocate another image.
    let unpadded = payload.trim_end_matches('=');
    let padding = payload.len() - unpadded.len();
    if padding > 2 || !unpadded.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/') {
        return false;
    }
    let last = match unpadded.as_bytes().last().copied() {
        Some(b'A'..=b'Z') => unpadded.as_bytes()[unpadded.len() - 1] - b'A',
        Some(b'a'..=b'z') => unpadded.as_bytes()[unpadded.len() - 1] - b'a' + 26,
        Some(b'0'..=b'9') => unpadded.as_bytes()[unpadded.len() - 1] - b'0' + 52,
        Some(b'+') => 62,
        Some(b'/') => 63,
        _ => return false,
    };
    match padding { 1 => last & 3 == 0, 2 => last & 15 == 0, _ => true }
}
