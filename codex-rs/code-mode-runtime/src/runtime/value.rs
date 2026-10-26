use serde_json::Value as JsonValue;

use codex_code_mode_protocol::DEFAULT_IMAGE_DETAIL;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::ImageDetail;

const IMAGE_HELPER_EXPECTS_MESSAGE: &str = "image expects a non-empty image URL string, an object with image_url and optional detail, or a raw MCP image block";
const REMOTE_IMAGE_URL_ERROR: &str = "Tool call failed: remote image URLs are not supported in tool outputs. Pass a base64 data URI instead";
const INVALID_IMAGE_URL_ERROR: &str =
    "Tool call failed: invalid image output. Pass a base64 data URI instead";
const CODEX_IMAGE_DETAIL_META_KEY: &str = "codex/imageDetail";
const EXECUTION_TERMINATED_MESSAGE: &str = "JavaScript execution terminated";

enum ImageOutputError {
    Discarded,
    Invalid(String),
}
impl From<String> for ImageOutputError {
    fn from(error: String) -> Self {
        Self::Invalid(error)
    }
}

// Data URIs have no JSON escapes. Include the largest detail spelling and the
// content-array separator, independently of the unrelated 8 MiB text limit.
const IMAGE_ENCODING_OVERHEAD: usize =
    br#"{"type":"input_image","image_url":"","detail":"original"},"#.len();

fn copy_image_url(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::String>,
    extra_bytes: usize,
) -> Result<String, ImageOutputError> {
    let bytes = value.utf8_length(scope).saturating_add(extra_bytes);
    if let Some(state) = scope.get_slot::<super::RuntimeState>()
        && let Some(event) = state
            .output_admission
            .reject_before_conversion_with_overhead(bytes, IMAGE_ENCODING_OVERHEAD)
    {
        if let Some(event) = event {
            let _ = state.event_tx.send(event);
        }
        return Err(ImageOutputError::Discarded);
    }
    Ok(copy_string(scope, value))
}

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
    if scope.is_execution_terminating() {
        return Err(EXECUTION_TERMINATED_MESSAGE.to_string());
    }
    if value.is_undefined()
        || value.is_null()
        || value.is_boolean()
        || value.is_number()
        || value.is_big_int()
        || value.is_string()
    {
        let value = value
            .to_string(scope)
            .ok_or_else(|| "failed to format text".to_string())?;
        return bounded_string(scope, value, max_bytes);
    }

    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    if let Some(stringified) = super::output_projection::stringify(&mut tc, value) {
        return bounded_string(&mut tc, stringified, max_bytes);
    }
    if tc.is_execution_terminating() {
        return Err(EXECUTION_TERMINATED_MESSAGE.to_string());
    }
    if tc.has_caught() {
        return Err(tc
            .exception()
            .map(|exception| value_to_error_text(&mut tc, exception))
            .unwrap_or_else(|| "unknown code mode exception".to_string()));
    }
    let value = value
        .to_string(&tc)
        .ok_or_else(|| "failed to format text".to_string())?;
    bounded_string(&mut tc, value, max_bytes)
}

pub(super) fn serialize_console_text(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    max_bytes: usize,
) -> Result<String, String> {
    if scope.is_execution_terminating() {
        return Err(EXECUTION_TERMINATED_MESSAGE.to_string());
    }
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let has_evidence = if value.is_native_error()
        && let Ok(object) = v8::Local::<v8::Object>::try_from(value)
    {
        ["results", "evidence", "missing_capabilities"].iter().any(|field| {
            if tc.is_execution_terminating() {
                return false;
            }
            v8::String::new(&tc, field)
                .and_then(|key| object.get(&tc, key.into()))
                .is_some_and(|value| !value.is_undefined())
        })
    } else {
        false
    };
    if tc.is_execution_terminating() {
        return Err(EXECUTION_TERMINATED_MESSAGE.to_string());
    }
    if has_evidence {
        tc.reset();
        return serialize_output_text_with_limit(&mut tc, value, max_bytes);
    }
    tc.reset();
    if value.is_native_error()
        && let Ok(object) = v8::Local::<v8::Object>::try_from(value)
        && let Some(key) = v8::String::new(&tc, "stack")
        && let Some(stack) = object.get(&tc, key.into())
        && let Ok(stack) = v8::Local::<v8::String>::try_from(stack)
    {
        if tc.is_execution_terminating() {
            return Err(EXECUTION_TERMINATED_MESSAGE.to_string());
        }
        return bounded_string(&mut tc, stack, max_bytes);
    }
    if tc.is_execution_terminating() {
        return Err(EXECUTION_TERMINATED_MESSAGE.to_string());
    }
    tc.reset();
    if value.is_object() {
        if let Some(stringified) = super::output_projection::stringify(&mut tc, value) {
            return bounded_string(&mut tc, stringified, max_bytes);
        }
        if tc.is_execution_terminating() {
            return Err(EXECUTION_TERMINATED_MESSAGE.to_string());
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
    let result = (|| -> Result<FunctionCallOutputContentItem, ImageOutputError> {
        let (image_url, detail) = if let Ok(image_url) = v8::Local::<v8::String>::try_from(value) {
            (copy_image_url(scope, image_url, 0)?, None)
        } else if value.is_object() && !value.is_array() {
            let object = v8::Local::<v8::Object>::try_from(value)
                .map_err(|_| IMAGE_HELPER_EXPECTS_MESSAGE.to_string())?;
            if let Some(image) = parse_non_mcp_output_image(scope, object)? {
                image
            } else {
                parse_mcp_output_image(scope, value)?
            }
        } else {
            return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string().into());
        };

        if image_url.is_empty() {
            return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string().into());
        }
        let Some((scheme, _)) = image_url.split_once(':') else {
            return Err(INVALID_IMAGE_URL_ERROR.to_string().into());
        };
        if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            return Err(REMOTE_IMAGE_URL_ERROR.to_string().into());
        }
        if !scheme.eq_ignore_ascii_case("data") || !valid_image_data_uri(&image_url) {
            return Err(INVALID_IMAGE_URL_ERROR.to_string().into());
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
                        return Err("image detail must be one of: auto, low, high, original"
                            .to_string()
                            .into());
                    }
                })
            }
            None => Some(DEFAULT_IMAGE_DETAIL),
        };

        Ok(FunctionCallOutputContentItem::InputImage { image_url, detail })
    })();

    match result {
        Ok(item) => Ok(item),
        Err(ImageOutputError::Discarded) => Err(()),
        Err(ImageOutputError::Invalid(error_text)) => {
            throw_type_error(scope, &error_text);
            Err(())
        }
    }
}

fn parse_non_mcp_output_image(
    scope: &mut v8::PinScope<'_, '_>,
    object: v8::Local<'_, v8::Object>,
) -> Result<Option<(String, Option<String>)>, ImageOutputError> {
    let image_url_key = v8::String::new(scope, "image_url")
        .ok_or_else(|| "failed to allocate image helper keys".to_string())?;
    let Some(image_url) = object.get(scope, image_url_key.into()) else {
        return Ok(None);
    };
    if image_url.is_undefined() {
        return Ok(None);
    }
    let image_url = v8::Local::<v8::String>::try_from(image_url)
        .map_err(|_| IMAGE_HELPER_EXPECTS_MESSAGE.to_string())?;
    let image_url = copy_image_url(scope, image_url, 0)?;
    let detail_key = v8::String::new(scope, "detail")
        .ok_or_else(|| "failed to allocate image helper keys".to_string())?;
    let detail = parse_image_detail_value(scope, object.get(scope, detail_key.into()))?;
    Ok(Some((image_url, detail)))
}

fn image_property<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    object: v8::Local<'_, v8::Object>,
    key: &str,
) -> Result<v8::Local<'s, v8::Value>, String> {
    let key = v8::String::new(scope, key)
        .ok_or_else(|| "failed to allocate image helper key".to_string())?;
    object
        .get(scope, key.into())
        .ok_or_else(|| "failed to read image helper property".to_string())
}

fn parse_mcp_output_image(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<(String, Option<String>), ImageOutputError> {
    let object = v8::Local::<v8::Object>::try_from(value)
        .map_err(|_| IMAGE_HELPER_EXPECTS_MESSAGE.to_string())?;
    let kind = image_property(scope, object, "type")?;
    let kind = v8::Local::<v8::String>::try_from(kind)
        .map_err(|_| IMAGE_HELPER_EXPECTS_MESSAGE.to_string())?;
    let kind = bounded_string(scope, kind, 32)?;
    if kind != "image" {
        return Err(format!("image only accepts MCP image blocks, got \"{kind}\"").into());
    }
    let data = image_property(scope, object, "data")?;
    let data = v8::Local::<v8::String>::try_from(data)
        .map_err(|_| "image expected MCP image data".to_string())?;
    // Reject before Rust conversion even when the raw MCP payload is huge.
    let data = copy_image_url(scope, data, 160)?;
    if data.is_empty() {
        return Err("image expected MCP image data".to_string().into());
    }
    let mime = image_property(scope, object, "mimeType")?;
    let mime = if mime.is_undefined() {
        image_property(scope, object, "mime_type")?
    } else {
        mime
    };
    let mime = if let Ok(mime) = v8::Local::<v8::String>::try_from(mime) {
        bounded_string(scope, mime, 128)?
    } else {
        String::new()
    };
    let mime = if mime.is_empty() {
        "application/octet-stream"
    } else {
        &mime
    };
    let image_url = if data
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        data
    } else {
        format!("data:{mime};base64,{data}")
    };
    let meta = image_property(scope, object, "_meta")?;
    let detail = if let Ok(meta) = v8::Local::<v8::Object>::try_from(meta) {
        let key = v8::String::new(scope, CODEX_IMAGE_DETAIL_META_KEY)
            .ok_or_else(|| "failed to allocate image helper key".to_string())?;
        let detail = meta.get(scope, key.into());
        match detail.and_then(|d| v8::Local::<v8::String>::try_from(d).ok()) {
            Some(detail) => bounded_string(scope, detail, 8)
                .ok()
                .filter(|d| matches!(d.as_str(), "auto" | "low" | "high" | "original")),
            None => None,
        }
    } else {
        None
    };
    Ok((image_url, detail))
}

fn parse_image_detail_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: Option<v8::Local<'s, v8::Value>>,
) -> Result<Option<String>, String> {
    match value {
        Some(value) if value.is_null() || value.is_undefined() => Ok(None),
        Some(value) => match v8::Local::<v8::String>::try_from(value) {
            Ok(value) => bounded_string(scope, value, 8).map(Some),
            Err(_) => Err("image detail must be a string when provided".to_string()),
        },
        None => Ok(None),
    }
}

pub(super) fn v8_value_to_json(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<Option<JsonValue>, String> {
    v8_value_to_json_with_limit(scope, value, MAX_PAYLOAD_BYTES).map_err(|error| match error {
        JsonConversionError::TooLarge => {
            format!("payload exceeds its limit of {MAX_PAYLOAD_BYTES} bytes")
        }
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
    if scope.is_execution_terminating() {
        return Err(JsonConversionError::Invalid(
            EXECUTION_TERMINATED_MESSAGE.to_string(),
        ));
    }
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let encoded = super::output_projection::bounded_json_codec(&mut tc, value, max_bytes);
    if tc.is_execution_terminating() {
        return Err(JsonConversionError::Invalid(
            EXECUTION_TERMINATED_MESSAGE.to_string(),
        ));
    }
    if encoded.is_some_and(|value| value.is_null()) {
        return Err(JsonConversionError::TooLarge);
    }
    let Some(stringified) = encoded.and_then(|value| v8::Local::<v8::String>::try_from(value).ok())
    else {
        if tc.has_caught() {
            return Err(JsonConversionError::Invalid(
                tc.exception()
                    .map(|exception| value_to_error_text(&mut tc, exception))
                    .unwrap_or_else(|| "unknown code mode exception".to_string()),
            ));
        }
        return Ok(None);
    };
    if stringified.utf8_length(&tc) > max_bytes {
        return Err(JsonConversionError::TooLarge);
    }
    serde_json::from_str(&copy_string(&mut tc, stringified))
        .map(Some)
        .map_err(|err| {
            JsonConversionError::Invalid(format!("failed to serialize JavaScript value: {err}"))
        })
}

pub(super) fn json_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &JsonValue,
) -> Option<v8::Local<'s, v8::Value>> {
    let json = serde_json::to_string(value).ok()?;
    serialized_json_to_v8(scope, &json)
}

pub(super) fn serialized_json_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    if scope.is_execution_terminating() {
        return None;
    }
    let json = v8::String::new(scope, json)?;
    super::output_projection::json_codec(scope, json.into(), "parse")
}

fn diagnostic_string(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<'_, v8::String>) -> String {
    let limit = super::MAX_ERROR_TEXT_BYTES;
    if value.utf8_length(scope) <= limit {
        return copy_string(scope, value);
    }
    let mut bytes = vec![0; limit - super::ERROR_TRUNCATION_SUFFIX.len()];
    let written = value.write_utf8_v2(scope, &mut bytes, v8::WriteFlags::kReplaceInvalidUtf8, None);
    bytes.truncate(written);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    text.push_str(super::ERROR_TRUNCATION_SUFFIX);
    text
}

pub(super) fn value_to_error_text(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> String {
    if scope.is_execution_terminating() {
        return EXECUTION_TERMINATED_MESSAGE.to_string();
    }
    // Captured before the TryCatch borrows the scope. Used only to repair a
    // call to a nested tool name that this cell's catalog does not contain.
    let enabled_tools = scope
        .get_slot::<super::RuntimeState>()
        .map(|state| std::sync::Arc::clone(&state.enabled_tools));
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let object = v8::Local::<v8::Object>::try_from(value).ok();
    let stack = object.and_then(|object| {
        let key = v8::String::new(&tc, "stack")?;
        v8::Local::<v8::String>::try_from(object.get(&tc, key.into())?).ok()
    });
    if tc.is_execution_terminating() {
        return EXECUTION_TERMINATED_MESSAGE.to_string();
    }
    tc.reset();
    let string = stack.or_else(|| value.to_string(&tc));
    if tc.is_execution_terminating() {
        return EXECUTION_TERMINATED_MESSAGE.to_string();
    }
    tc.reset();
    let mut text = string
        .map(|string| diagnostic_string(&mut tc, string))
        .unwrap_or_else(|| "[unprintable exception]".to_string());
    if text.lines().next().is_some_and(|line| {
        line.starts_with("TypeError: ") && line.ends_with(".index is not a function")
    }) {
        text.push_str("\nHint: JavaScript strings use .indexOf(value), not Python's .index(value); a missing match returns -1.");
    }
    // A guessed tool name is the most common invalid nested call. Name the
    // catalog's candidates instead of returning a bare TypeError.
    let missing_tool = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("TypeError: tools."))
        .and_then(|rest| rest.strip_suffix(" is not a function"))
        .filter(|name| !name.is_empty() && !name.contains(char::is_whitespace))
        .map(str::to_owned);
    if let Some(name) = missing_tool
        && let Some(catalog) = enabled_tools.as_deref()
        && catalog.resolve_requested_name(&name).is_none()
    {
        let diagnostic = catalog.resolution_diagnostic(Some(name.as_str()));
        text.push_str(&format!(
            "\nHint: no nested tool named `{name}` is enabled in this cell; it did not run. Resolution: {diagnostic}"
        ));
    }
    // Uncaught helper failures must carry their settled receipts through the
    // existing canonical cell-output artifact, not just the small nested-call
    // fallback. Do not run this on successful results or emit extra tool calls.
    // A run_graph capability preflight carries its missing names instead.
    if let Some(object) = object {
        for field in [
            "results",
            "evidence",
            "missing_capabilities",
            "omitted_capabilities",
        ] {
            let Some(key) = v8::String::new(&tc, field) else {
                continue;
            };
            let attached = object.get(&tc, key.into());
            if tc.is_execution_terminating() {
                return EXECUTION_TERMINATED_MESSAGE.to_string();
            }
            tc.reset();
            let Some(attached) = attached.filter(|value| !value.is_undefined()) else {
                continue;
            };
            let remaining = MAX_PAYLOAD_BYTES.saturating_sub(text.len() + field.len() + 32);
            let mut serialized = if field == "results" {
                // Graph receipts are canonical evidence, not a tool-display
                // projection. One JSON node per line allows selective recovery
                // through the existing cell-output artifact without replay.
                super::output_projection::bounded_graph_json_codec(&mut tc, attached, remaining)
                    .and_then(|value| v8::Local::<v8::String>::try_from(value).ok())
                    .and_then(|json| bounded_string(&mut tc, json, remaining).ok())
                    .and_then(|json| {
                        let value = serde_json::from_str::<JsonValue>(&json).ok()?;
                        let nodes = value.as_object()?;
                        let rows = nodes.iter().map(|(id, node)| {
                            format!("{}:{}", serde_json::to_string(id).unwrap_or_default(), node)
                        }).collect::<Vec<_>>();
                        let indexed = format!("{{\n{}\n}}", rows.join(",\n"));
                        // Framing must not exceed the existing error-evidence
                        // cap. Keep exact compact JSON at that rare boundary.
                        Some(if indexed.len() <= remaining { indexed } else { json })
                    })
            } else {
                super::output_projection::stringify(&mut tc, attached)
                    .and_then(|json| bounded_string(&mut tc, json, remaining).ok())
            };
            if tc.is_execution_terminating() {
                return EXECUTION_TERMINATED_MESSAGE.to_string();
            }
            tc.reset();
            if serialized.is_none() {
                serialized =
                    super::output_projection::json_codec(&mut tc, attached, "helper_evidence")
                        .and_then(|value| v8::Local::<v8::String>::try_from(value).ok())
                        .and_then(|json| bounded_string(&mut tc, json, remaining).ok());
                if tc.is_execution_terminating() {
                    return EXECUTION_TERMINATED_MESSAGE.to_string();
                }
                tc.reset();
            }
            text.push_str(&format!("\nHelper {field}:\n"));
            text.push_str(serialized.as_deref().unwrap_or(
                "[evidence unavailable: unserializable or exceeds remaining 8 MiB error budget]",
            ));
        }
    }
    text
}

pub(super) fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    // Allocating/throwing a catchable exception must not replace termination.
    if scope.is_execution_terminating() {
        return;
    }
    if let Some(message) = v8::String::new(scope, message) {
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
    }
}

pub(super) fn error_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    message: &str,
) -> v8::Local<'s, v8::Value> {
    if scope.is_execution_terminating() {
        return v8::undefined(scope).into();
    }
    match v8::String::new(scope, message) {
        Some(message) => v8::Exception::error(scope, message),
        None => v8::undefined(scope).into(),
    }
}

fn valid_image_data_uri(uri: &str) -> bool {
    let Some((header, payload)) = uri.split_once(',') else {
        return false;
    };
    let Some((mime, encoding)) = header.get(5..).and_then(|header| header.split_once(';')) else {
        return false;
    };
    // view_image and MCP blocks without a MIME type return opaque source bytes.
    // The history insertion path decodes and validates those bytes as an image.
    if ![
        "image/png",
        "image/jpeg",
        "image/webp",
        "image/gif",
        "application/octet-stream",
    ]
    .iter()
    .any(|supported| mime.eq_ignore_ascii_case(supported))
        || !encoding.eq_ignore_ascii_case("base64")
        || payload.is_empty()
        || payload.len() % 4 != 0
    {
        return false;
    }
    // Validate canonical base64 in place; decoding would allocate another image.
    let unpadded = payload.trim_end_matches('=');
    let padding = payload.len() - unpadded.len();
    if padding > 2
        || !unpadded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
    {
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
    match padding {
        1 => last & 3 == 0,
        2 => last & 15 == 0,
        _ => true,
    }
}
