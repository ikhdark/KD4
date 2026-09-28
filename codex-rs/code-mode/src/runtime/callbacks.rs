use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::MAX_TOOL_TIMEOUT_MS;
use std::sync::Arc;

use super::EXIT_SENTINEL;
use super::MAX_OUTSTANDING_CALLBACKS_PER_CELL;
use super::MAX_SESSION_STORED_VALUE_BYTES;
use super::MAX_SESSION_STORED_VALUES;
use super::RuntimeEvent;
use super::RuntimeState;
use super::StoredValue;
use super::stored_value_entry_bytes;
use super::stored_value_limit_message;
use super::timers;
use super::value::json_to_v8;
use super::value::normalize_output_image;
use super::value::serialize_output_text;
use super::value::throw_type_error;
use super::value::v8_value_to_json;

pub(super) fn tool_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let tool_index = match v8::Local::<v8::Uint32>::try_from(args.data()) {
        Ok(tool_index) => tool_index.value() as usize,
        Err(_) => {
            throw_type_error(scope, "invalid tool callback data");
            return;
        }
    };
    if reject_unavailable_callback(scope, &mut retval) {
        return;
    }
    let input = if args.length() == 0 {
        Ok(None)
    } else {
        v8_value_to_json(scope, args.get(0))
    };
    let input = match input {
        Ok(input) => input,
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };
    let timeout_ms = match tool_timeout_ms(scope, args, tool_index) {
        Ok(timeout_ms) => timeout_ms,
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };

    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw_type_error(scope, "failed to create tool promise");
        return;
    };
    let promise = resolver.get_promise(scope);

    if scope
        .get_slot::<RuntimeState>()
        .is_some_and(|state| !state.can_admit_callback())
    {
        reject_callback_limit(scope, resolver, promise, &mut retval);
        return;
    }

    let resolver = v8::Global::new(scope, resolver);
    let (tool_name, tool_kind) = {
        let Some(state) = scope.get_slot::<RuntimeState>() else {
            throw_type_error(scope, "runtime state unavailable");
            return;
        };
        let Some(tool) = state.enabled_tools.get(tool_index) else {
            throw_type_error(scope, "tool callback data is out of range");
            return;
        };
        (tool.tool_name.clone(), tool.kind)
    };

    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        throw_type_error(scope, "runtime state unavailable");
        return;
    };
    let id = format!("tool-{}", state.next_tool_call_id);
    state.next_tool_call_id = state.next_tool_call_id.saturating_add(1);
    let event_tx = state.event_tx.clone();
    state.pending_tool_calls.insert(id.clone(), resolver);
    if event_tx.send(RuntimeEvent::ToolCall {
        id: id.clone(),
        name: tool_name,
        kind: tool_kind,
        input,
        timeout_ms,
    }).is_err() {
        state.pending_tool_calls.remove(&id);
        scope.terminate_execution();
    }
    retval.set(promise.into());
}

pub(super) fn text_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let value = if args.length() == 0 {
        v8::undefined(scope).into()
    } else {
        args.get(0)
    };
    if skip_output_conversion(scope, value) {
        return;
    }
    let text = match serialize_output_text(scope, value) {
        Ok(text) => text,
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        state.emit_output(FunctionCallOutputContentItem::InputText { text });
    }
    retval.set(v8::undefined(scope).into());
}

/// `console.log(...)` and its siblings forward to `text(...)`, joining every
/// argument with one space the way a terminal would. Scripts written for Node
/// then produce model-visible output instead of a `console is not defined`
/// failure that costs a whole model round.
pub(super) fn console_log_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    if skip_output_conversion(scope, args.get(0)) {
        return;
    }
    let mut output = String::new();
    for index in 0..args.length() {
        if index != 0 {
            output.push(' ');
        }
        let remaining = super::value::MAX_PAYLOAD_BYTES.saturating_sub(output.len());
        match super::value::serialize_console_text(scope, args.get(index), remaining) {
            Ok(text) => output.push_str(&text),
            Err(error_text) => {
                throw_type_error(scope, &error_text);
                return;
            }
        }
    }
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        state.emit_output(FunctionCallOutputContentItem::InputText {
            text: output,
        });
    }
    retval.set(v8::undefined(scope).into());
}

pub(super) fn image_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let value = if args.length() == 0 {
        v8::undefined(scope).into()
    } else {
        args.get(0)
    };
    let detail_override = if args.length() < 2 {
        None
    } else {
        let detail = args.get(1);
        if detail.is_string() {
            Some(detail.to_rust_string_lossy(scope))
        } else if detail.is_null() || detail.is_undefined() {
            None
        } else {
            throw_type_error(scope, "image detail must be a string when provided");
            return;
        }
    };
    let image_item = match normalize_output_image(scope, value, detail_override) {
        Ok(image_item) => image_item,
        Err(()) => return,
    };
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        state.emit_output(image_item);
    }
    retval.set(v8::undefined(scope).into());
}

pub(super) fn generated_image_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let value = if args.length() == 0 {
        v8::undefined(scope).into()
    } else {
        args.get(0)
    };
    let output_hint = match generated_image_output_hint(scope, value) {
        Ok(output_hint) => output_hint,
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };
    let image_item = match normalize_output_image(scope, value, /*detail_override*/ None) {
        Ok(image_item) => image_item,
        Err(()) => return,
    };
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        state.emit_output(image_item);
        if let Some(text) = output_hint {
            state.emit_output(FunctionCallOutputContentItem::InputText { text });
        }
    }
    retval.set(v8::undefined(scope).into());
}

fn generated_image_output_hint(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<Option<String>, String> {
    let object = v8::Local::<v8::Object>::try_from(value)
        .map_err(|_| "generatedImage expects an image generation result object".to_string())?;
    let key = v8::String::new(scope, "output_hint")
        .ok_or_else(|| "failed to allocate generatedImage helper keys".to_string())?;
    let output_hint = object
        .get(scope, key.into())
        .ok_or_else(|| "failed to read generatedImage output_hint".to_string())?;
    if output_hint.is_undefined() {
        return Ok(None);
    }
    if !output_hint.is_string() {
        return Err("generatedImage output_hint must be a string when provided".to_string());
    }
    Ok(Some(output_hint.to_rust_string_lossy(scope)))
}

pub(super) fn store_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue<v8::Value>,
) {
    let key = match args.get(0).to_string(scope) {
        Some(key) => match super::value::bounded_string(scope, key, MAX_SESSION_STORED_VALUE_BYTES) {
            Ok(key) => key,
            Err(_) => {
                reject_storage_limit(scope);
                return;
            }
        },
        None => {
            throw_type_error(scope, "store key must be a string");
            return;
        }
    };
    let admission = scope.get_slot::<RuntimeState>().and_then(|state| {
        if state.stored_value_limit_error.is_some()
            || (!state.stored_values.contains_key(&key) && state.stored_values.len() >= MAX_SESSION_STORED_VALUES) {
            return None;
        }
        let key_bytes = stored_value_entry_bytes(&key, &serde_json::Value::Null).saturating_sub(4);
        MAX_SESSION_STORED_VALUE_BYTES.checked_sub(state.total_stored_value_bytes)
            .and_then(|available| available.checked_add(state.stored_values.get(&key).map_or(0, |stored| stored.bytes)))
            .and_then(|available| available.checked_sub(key_bytes))
    });
    let Some(max_bytes) = admission else {
        reject_storage_limit(scope);
        return;
    };
    let value = args.get(1);
    let serialized = match super::value::v8_value_to_json_with_limit(scope, value, max_bytes) {
        Ok(Some(value)) => value,
        Ok(None) => {
            throw_type_error(
                scope,
                &format!("Unable to store {key:?}. Only plain serializable objects can be stored."),
            );
            return;
        }
        Err(super::value::JsonConversionError::TooLarge) => {
            reject_storage_limit(scope);
            return;
        }
        Err(super::value::JsonConversionError::Invalid(error_text)) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };
    let limit_error = scope.get_slot_mut::<RuntimeState>().and_then(|state| {
        if let Some(error) = state.stored_value_limit_error.as_ref() {
            return Some(error.clone());
        }

        let stored = StoredValue::new(&key, serialized);
        let previous_bytes = state.stored_values.get(&key).map(|previous| previous.bytes);
        let total_bytes = state
            .total_stored_value_bytes
            .checked_sub(previous_bytes.unwrap_or(0))
            .and_then(|total| total.checked_add(stored.bytes));
        if state.stored_values.len() + usize::from(previous_bytes.is_none())
            <= MAX_SESSION_STORED_VALUES
            && let Some(total_bytes) =
                total_bytes.filter(|total| *total <= MAX_SESSION_STORED_VALUE_BYTES)
        {
            state.total_stored_value_bytes = total_bytes;
            state.stored_values.insert(key.clone(), stored.clone());
            state.stored_value_writes.insert(key, stored);
            return None;
        }

        state.stored_value_writes.clear();
        let error = stored_value_limit_message();
        state.stored_value_limit_error = Some(error.clone());
        Some(error)
    });
    if let Some(error) = limit_error {
        throw_type_error(scope, &error);
    }
}

fn reject_storage_limit(scope: &mut v8::PinScope<'_, '_>) {
    let error = stored_value_limit_message();
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        state.stored_value_writes.clear();
        state.stored_value_limit_error = Some(error.clone());
    }
    throw_type_error(scope, &error);
}

pub(super) fn load_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let key = match args.get(0).to_string(scope) {
        Some(key) => key.to_rust_string_lossy(scope),
        None => {
            throw_type_error(scope, "load key must be a string");
            return;
        }
    };
    let value = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.stored_values.get(&key))
        .map(|stored| Arc::clone(&stored.value));
    let Some(value) = value else {
        retval.set(v8::undefined(scope).into());
        return;
    };
    let Some(value) = json_to_v8(scope, &value) else {
        throw_type_error(scope, "failed to load stored value");
        return;
    };
    retval.set(value);
}

pub(super) fn notify_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    if reject_unavailable_callback(scope, &mut retval) {
        return;
    }
    let value = if args.length() == 0 {
        v8::undefined(scope).into()
    } else {
        args.get(0)
    };
    let text = match super::value::serialize_output_text_with_limit(scope, value, super::value::MAX_NOTIFICATION_BYTES) {
        Ok(text) => text,
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };
    if text.trim().is_empty() {
        throw_type_error(scope, "notify expects non-empty text");
        return;
    }
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw_type_error(scope, "failed to create notification promise");
        return;
    };
    let promise = resolver.get_promise(scope);
    if scope
        .get_slot::<RuntimeState>()
        .is_some_and(|state| !state.can_admit_callback())
    {
        reject_callback_limit(scope, resolver, promise, &mut retval);
        return;
    }
    let resolver = v8::Global::new(scope, resolver);
    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        throw_type_error(scope, "runtime state unavailable");
        return;
    };
    let id = format!("notify-{}", state.next_notification_id);
    state.next_notification_id = state.next_notification_id.saturating_add(1);
    state.pending_notifications.insert(id.clone(), resolver);
    if state.event_tx.send(RuntimeEvent::Notify {
        id: Some(id.clone()),
        call_id: state.tool_call_id.clone(),
        text,
    }).is_err() {
        state.pending_notifications.remove(&id);
        scope.terminate_execution();
    }
    retval.set(promise.into());
}

fn skip_output_conversion(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<'_, v8::Value>) -> bool {
    let bytes = v8::Local::<v8::String>::try_from(value).map(|value| value.utf8_length(scope)).unwrap_or(0);
    let Some(state) = scope.get_slot::<RuntimeState>() else { return false; };
    let Some(event) = state.output_admission.reject_before_conversion(bytes) else { return false; };
    if let Some(event) = event {
        let _ = state.event_tx.send(event);
    }
    true
}

fn reject_unavailable_callback(
    scope: &mut v8::PinScope<'_, '_>,
    retval: &mut v8::ReturnValue<v8::Value>,
) -> bool {
    if scope.get_slot::<RuntimeState>().is_some_and(|state| state.event_tx.is_closed()) {
        scope.terminate_execution();
        return true;
    }
    if scope.get_slot::<RuntimeState>().is_some_and(|state| !state.can_admit_callback()) {
        if let Some(resolver) = v8::PromiseResolver::new(scope) {
            let promise = resolver.get_promise(scope);
            reject_callback_limit(scope, resolver, promise, retval);
        } else {
            throw_type_error(scope, "failed to create callback promise");
        }
        return true;
    }
    false
}

fn reject_callback_limit(
    scope: &mut v8::PinScope<'_, '_>,
    resolver: v8::Local<'_, v8::PromiseResolver>,
    promise: v8::Local<'_, v8::Promise>,
    retval: &mut v8::ReturnValue<v8::Value>,
) {
    let message = format!(
        "code mode cell exceeded its limit of {MAX_OUTSTANDING_CALLBACKS_PER_CELL} outstanding tool and notification callbacks"
    );
    let error = super::value::error_value(scope, &message);
    resolver.reject(scope, error);
    retval.set(promise.into());
}

fn tool_timeout_ms(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    tool_index: usize,
) -> Result<u64, String> {
    let default_timeout_ms = scope
        .get_slot::<RuntimeState>()
        .map(|state| {
            state
                .enabled_tools
                .get(tool_index)
                .and_then(|tool| tool.default_timeout_ms)
                .unwrap_or(state.default_tool_timeout_ms)
                .clamp(1, MAX_TOOL_TIMEOUT_MS)
        })
        .ok_or_else(|| "runtime state unavailable".to_string())?;
    if args.length() < 2 || args.get(1).is_undefined() {
        return Ok(default_timeout_ms);
    }
    let options = v8::Local::<v8::Object>::try_from(args.get(1))
        .map_err(|_| "nested tool options must be an object containing `timeout_ms`".to_string())?;
    let key = v8::String::new(scope, "timeout_ms")
        .ok_or_else(|| "failed to allocate nested tool option key".to_string())?;
    let value = options
        .get(scope, key.into())
        .ok_or_else(|| "failed to read nested tool timeout_ms".to_string())?;
    if value.is_undefined() {
        return Ok(default_timeout_ms);
    }
    let timeout_ms = value
        .number_value(scope)
        .ok_or_else(|| "nested tool timeout_ms must be a positive integer".to_string())?;
    if !timeout_ms.is_finite()
        || timeout_ms.fract() != 0.0
        || timeout_ms < 1.0
        || timeout_ms > MAX_TOOL_TIMEOUT_MS as f64
    {
        return Err(format!(
            "nested tool timeout_ms must be a positive integer no greater than {MAX_TOOL_TIMEOUT_MS}"
        ));
    }
    Ok(timeout_ms as u64)
}

pub(super) fn set_timeout_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let timeout_id = match timers::schedule_timeout(scope, args) {
        Ok(timeout_id) => timeout_id,
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };

    retval.set(v8::Number::new(scope, timeout_id as f64).into());
}

pub(super) fn clear_timeout_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    if let Err(error_text) = timers::clear_timeout(scope, args) {
        throw_type_error(scope, &error_text);
        return;
    }

    retval.set(v8::undefined(scope).into());
}

pub(super) fn yield_control_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue<v8::Value>,
) {
    if let Some(state) = scope.get_slot::<RuntimeState>()
        && let Some(event) = state.output_admission.admit_yield()
    {
        let _ = state.event_tx.send(event);
    }
}

pub(super) fn exit_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue<v8::Value>,
) {
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        state.exit_requested = true;
    }
    if let Some(error) = v8::String::new(scope, EXIT_SENTINEL) {
        scope.throw_exception(error.into());
    }
}
