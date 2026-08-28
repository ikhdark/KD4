use codex_code_mode_protocol::FunctionCallOutputContentItem;

use super::EXIT_SENTINEL;
use super::MAX_OUTSTANDING_CALLBACKS_PER_CELL;
use super::RuntimeEvent;
use super::RuntimeState;
use super::stored_value_limit_message;
use super::stored_values_within_limits;
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
    let tool_index = match args.data().to_rust_string_lossy(scope).parse::<usize>() {
        Ok(tool_index) => tool_index,
        Err(_) => {
            throw_type_error(scope, "invalid tool callback data");
            return;
        }
    };
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
    let timeout_ms = match tool_timeout_ms(scope, args) {
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
    let _ = event_tx.send(RuntimeEvent::ToolCall {
        id,
        name: tool_name,
        kind: tool_kind,
        input,
        timeout_ms,
    });
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
        Some(key) => key.to_rust_string_lossy(scope),
        None => {
            throw_type_error(scope, "store key must be a string");
            return;
        }
    };
    let value = args.get(1);
    let serialized = match v8_value_to_json(scope, value) {
        Ok(Some(value)) => value,
        Ok(None) => {
            throw_type_error(
                scope,
                &format!("Unable to store {key:?}. Only plain serializable objects can be stored."),
            );
            return;
        }
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };
    let limit_error = scope.get_slot_mut::<RuntimeState>().and_then(|state| {
        if let Some(error) = state.stored_value_limit_error.as_ref() {
            return Some(error.clone());
        }

        let previous = state.stored_values.insert(key.clone(), serialized.clone());
        if stored_values_within_limits(&state.stored_values) {
            state.stored_value_writes.insert(key, serialized);
            return None;
        }

        match previous {
            Some(previous) => {
                state.stored_values.insert(key, previous);
            }
            None => {
                state.stored_values.remove(&key);
            }
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
        .cloned();
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
    let value = if args.length() == 0 {
        v8::undefined(scope).into()
    } else {
        args.get(0)
    };
    let text = match serialize_output_text(scope, value) {
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
    let _ = state.event_tx.send(RuntimeEvent::Notify {
        id: Some(id),
        call_id: state.tool_call_id.clone(),
        text,
    });
    retval.set(promise.into());
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
    let error = v8::String::new(scope, &message)
        .map(Into::into)
        .unwrap_or_else(|| v8::undefined(scope).into());
    resolver.reject(scope, error);
    retval.set(promise.into());
}

fn tool_timeout_ms(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
) -> Result<u64, String> {
    const MAX_TOOL_TIMEOUT_MS: u64 = 30 * 60 * 1_000;

    let default_timeout_ms = scope
        .get_slot::<RuntimeState>()
        .map(|state| state.default_tool_timeout_ms)
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
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        let _ = state.event_tx.send(RuntimeEvent::YieldRequested);
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
