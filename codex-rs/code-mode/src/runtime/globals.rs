use std::sync::Arc;

use codex_code_mode_protocol::EnabledToolMetadata;

use super::EnabledToolCatalog;
use super::RuntimeState;
use super::callbacks::clear_timeout_callback;
use super::callbacks::console_log_callback;
use super::callbacks::exit_callback;
use super::callbacks::generated_image_callback;
use super::callbacks::image_callback;
use super::callbacks::load_callback;
use super::callbacks::notify_callback;
use super::callbacks::set_timeout_callback;
use super::callbacks::store_callback;
use super::callbacks::text_callback;
use super::callbacks::tool_callback;
use super::callbacks::yield_control_callback;
use super::value::throw_type_error;

/// Bare call forms the model reaches for inside a cell. Each forwards to the
/// `exec_command` nested tool when it is enabled; otherwise calling one throws
/// a TypeError that names the canonical `tools.<name>(...)` form and the
/// enabled tool names, so a single bad guess never loses the whole round.
const EXEC_COMMAND_ALIASES: &[&str] = &["exec", "exec_command", "execTool", "shell"];
const EXEC_COMMAND_GLOBAL_NAME: &str = "exec_command";
const CONSOLE_METHODS: &[&str] = &["log", "info", "warn", "error", "debug"];

pub(super) fn install_globals(scope: &mut v8::PinScope<'_, '_>) -> Result<(), String> {
    let global = scope.get_current_context().global(scope);
    delete_global(scope, global, "console")?;
    delete_global(scope, global, "Atomics")?;
    delete_global(scope, global, "SharedArrayBuffer")?;
    delete_global(scope, global, "WebAssembly")?;

    let enabled_tools = scope
        .get_slot::<RuntimeState>()
        .map(|state| Arc::clone(&state.enabled_tools))
        .unwrap_or_default();
    let tools = build_tools_object(scope, &enabled_tools.tools)?;
    let all_tools = build_all_tools_value(scope, &enabled_tools.tools)?;
    let all_tool_names = build_all_tool_names_value(scope, &enabled_tools.tools)?;
    let resolve_tool = helper_function(scope, "resolve_tool", resolve_tool_callback)?;
    let clear_timeout = helper_function(scope, "clearTimeout", clear_timeout_callback)?;
    let set_timeout = helper_function(scope, "setTimeout", set_timeout_callback)?;
    let text = helper_function(scope, "text", text_callback)?;
    let image = helper_function(scope, "image", image_callback)?;
    let generated_image = helper_function(scope, "generatedImage", generated_image_callback)?;
    let store = helper_function(scope, "store", store_callback)?;
    let load = helper_function(scope, "load", load_callback)?;
    let notify = helper_function(scope, "notify", notify_callback)?;
    let yield_control = helper_function(scope, "yield_control", yield_control_callback)?;
    let exit = helper_function(scope, "exit", exit_callback)?;

    set_global(scope, global, "tools", tools.into())?;
    set_global(scope, global, "ALL_TOOLS", all_tools)?;
    set_global(scope, global, "ALL_TOOL_NAMES", all_tool_names)?;
    set_global(scope, global, "resolve_tool", resolve_tool.into())?;
    set_global(scope, global, "clearTimeout", clear_timeout.into())?;
    set_global(scope, global, "setTimeout", set_timeout.into())?;
    set_global(scope, global, "text", text.into())?;
    set_global(scope, global, "image", image.into())?;
    set_global(scope, global, "generatedImage", generated_image.into())?;
    set_global(scope, global, "store", store.into())?;
    set_global(scope, global, "load", load.into())?;
    set_global(scope, global, "notify", notify.into())?;
    set_global(scope, global, "yield_control", yield_control.into())?;
    set_global(scope, global, "exit", exit.into())?;
    install_tool_aliases(scope, global, &enabled_tools)?;
    install_console_shim(scope, global)?;
    Ok(())
}

fn install_tool_aliases<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
    enabled_tools: &EnabledToolCatalog,
) -> Result<(), String> {
    let exec_command_index = enabled_tools.index_of(EXEC_COMMAND_GLOBAL_NAME);
    for alias in EXEC_COMMAND_ALIASES {
        // A real nested tool that happens to share an alias name keeps its own
        // identity on `tools`; only free names become forwarding aliases.
        if *alias != EXEC_COMMAND_GLOBAL_NAME && enabled_tools.index_of(alias).is_some() {
            continue;
        }
        let function = match exec_command_index {
            Some(index) => tool_function(scope, index)?,
            None => helper_function(scope, alias, missing_exec_alias_callback)?,
        };
        set_global(scope, global, alias, function.into())?;
    }
    Ok(())
}

fn install_console_shim<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
) -> Result<(), String> {
    let console = v8::Object::new(scope);
    for method in CONSOLE_METHODS {
        let function = helper_function(scope, method, console_log_callback)?;
        let key = v8::String::new(scope, method)
            .ok_or_else(|| format!("failed to allocate console.{method}"))?;
        if console.set(scope, key.into(), function.into()) != Some(true) {
            return Err(format!("failed to set console.{method}"));
        }
    }
    set_global(scope, global, "console", console.into())
}

fn missing_exec_alias_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue<v8::Value>,
) {
    let enabled_names = scope
        .get_slot::<RuntimeState>()
        .map(|state| {
            state
                .enabled_tools
                .tools
                .iter()
                .map(|tool| tool.global_name.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let listed = if enabled_names.is_empty() {
        "(no nested tools are enabled)".to_string()
    } else {
        enabled_names.join(", ")
    };
    throw_type_error(
        scope,
        &format!(
            "no `exec_command` nested tool is enabled in this cell, so `exec(...)` cannot run a command; call `await tools.<name>(...)` with one of: {listed}"
        ),
    );
}

fn build_all_tool_names_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    enabled_tools: &[EnabledToolMetadata],
) -> Result<v8::Local<'s, v8::Value>, String> {
    let array = v8::Array::new(scope, enabled_tools.len() as i32);
    for (index, tool) in enabled_tools.iter().enumerate() {
        let name = v8::String::new(scope, &tool.global_name)
            .ok_or_else(|| "failed to allocate ALL_TOOL_NAMES name".to_string())?;
        if array.set_index(scope, index as u32, name.into()) != Some(true) {
            return Err("failed to append ALL_TOOL_NAMES name".to_string());
        }
    }
    Ok(array.into())
}

fn resolve_tool_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    if args.length() == 0 || !args.get(0).is_string() {
        retval.set(v8::undefined(scope).into());
        return;
    }
    let requested_name = args.get(0).to_rust_string_lossy(scope);
    let enabled_tools = scope
        .get_slot::<RuntimeState>()
        .map(|state| Arc::clone(&state.enabled_tools));
    let Some(tool) = enabled_tools
        .as_deref()
        .and_then(|enabled_tools| enabled_tools.resolve(&requested_name))
    else {
        retval.set(v8::undefined(scope).into());
        return;
    };
    let item = v8::Object::new(scope);
    let Some(name_key) = v8::String::new(scope, "name") else {
        retval.set(v8::undefined(scope).into());
        return;
    };
    let Some(description_key) = v8::String::new(scope, "description") else {
        retval.set(v8::undefined(scope).into());
        return;
    };
    let Some(name) = v8::String::new(scope, &tool.global_name) else {
        retval.set(v8::undefined(scope).into());
        return;
    };
    let Some(description) = v8::String::new(scope, &tool.description) else {
        retval.set(v8::undefined(scope).into());
        return;
    };
    if item.set(scope, name_key.into(), name.into()) != Some(true)
        || item.set(scope, description_key.into(), description.into()) != Some(true)
    {
        retval.set(v8::undefined(scope).into());
        return;
    }
    retval.set(item.into());
}

fn build_tools_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    enabled_tools: &[EnabledToolMetadata],
) -> Result<v8::Local<'s, v8::Object>, String> {
    let tools = v8::Object::new(scope);

    for (tool_index, tool) in enabled_tools.iter().enumerate() {
        let name = v8::String::new(scope, &tool.global_name)
            .ok_or_else(|| "failed to allocate tool name".to_string())?;
        let function = tool_function(scope, tool_index)?;
        tools.set(scope, name.into(), function.into());
    }
    Ok(tools)
}

fn build_all_tools_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    enabled_tools: &[EnabledToolMetadata],
) -> Result<v8::Local<'s, v8::Value>, String> {
    let array = v8::Array::new(scope, enabled_tools.len() as i32);
    let name_key = v8::String::new(scope, "name")
        .ok_or_else(|| "failed to allocate ALL_TOOLS name key".to_string())?;
    let description_key = v8::String::new(scope, "description")
        .ok_or_else(|| "failed to allocate ALL_TOOLS description key".to_string())?;

    for (index, tool) in enabled_tools.iter().enumerate() {
        let item = v8::Object::new(scope);
        let name = v8::String::new(scope, &tool.global_name)
            .ok_or_else(|| "failed to allocate ALL_TOOLS name".to_string())?;
        let description = v8::String::new(scope, &tool.description)
            .ok_or_else(|| "failed to allocate ALL_TOOLS description".to_string())?;

        if item.set(scope, name_key.into(), name.into()) != Some(true) {
            return Err("failed to set ALL_TOOLS name".to_string());
        }
        if item.set(scope, description_key.into(), description.into()) != Some(true) {
            return Err("failed to set ALL_TOOLS description".to_string());
        }
        if array.set_index(scope, index as u32, item.into()) != Some(true) {
            return Err("failed to append ALL_TOOLS metadata".to_string());
        }
    }

    Ok(array.into())
}

fn helper_function<'s, F>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
    callback: F,
) -> Result<v8::Local<'s, v8::Function>, String>
where
    F: v8::MapFnTo<v8::FunctionCallback>,
{
    let name =
        v8::String::new(scope, name).ok_or_else(|| "failed to allocate helper name".to_string())?;
    let template = v8::FunctionTemplate::builder(callback)
        .data(name.into())
        .build(scope);
    template
        .get_function(scope)
        .ok_or_else(|| "failed to create helper function".to_string())
}

fn tool_function<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tool_index: usize,
) -> Result<v8::Local<'s, v8::Function>, String> {
    let data = v8::String::new(scope, &tool_index.to_string())
        .ok_or_else(|| "failed to allocate tool callback data".to_string())?;
    let template = v8::FunctionTemplate::builder(tool_callback)
        .data(data.into())
        .build(scope);
    template
        .get_function(scope)
        .ok_or_else(|| "failed to create tool function".to_string())
}

fn set_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
    name: &str,
    value: v8::Local<'s, v8::Value>,
) -> Result<(), String> {
    let key = v8::String::new(scope, name)
        .ok_or_else(|| format!("failed to allocate global `{name}`"))?;
    if global.set(scope, key.into(), value) == Some(true) {
        Ok(())
    } else {
        Err(format!("failed to set global `{name}`"))
    }
}

fn delete_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
    name: &str,
) -> Result<(), String> {
    let key = v8::String::new(scope, name)
        .ok_or_else(|| format!("failed to allocate global `{name}`"))?;
    if global.delete(scope, key.into()) == Some(true) {
        Ok(())
    } else {
        Err(format!("failed to remove global `{name}`"))
    }
}
