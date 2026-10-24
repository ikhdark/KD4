use std::sync::Arc;

use super::EnabledToolCatalog;
use super::RuntimeState;
use super::callbacks::clear_timeout_callback;
use super::callbacks::console_log_callback;
use super::callbacks::exit_callback;
use super::callbacks::format_tool_result_callback;
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
    super::output_projection::install(scope)?;

    let enabled_tools = scope
        .get_slot::<RuntimeState>()
        .map(|state| Arc::clone(&state.enabled_tools))
        .unwrap_or_default();
    let tools = build_tools_object(scope, &enabled_tools)?;
    let all_tool_names = build_all_tool_names_value(scope, &enabled_tools)?;
    let resolve_tool = helper_function(scope, "resolve_tool", resolve_tool_callback)?;
    let clear_timeout = helper_function(scope, "clearTimeout", clear_timeout_callback)?;
    let set_timeout = helper_function(scope, "setTimeout", set_timeout_callback)?;
    let text = helper_function(scope, "text", text_callback)?;
    let format_tool_result =
        helper_function(scope, "format_tool_result", format_tool_result_callback)?;
    let image = helper_function(scope, "image", image_callback)?;
    let generated_image = helper_function(scope, "generatedImage", generated_image_callback)?;
    let store = helper_function(scope, "store", store_callback)?;
    let load = helper_function(scope, "load", load_callback)?;
    let list_keys = helper_function(scope, "listKeys", super::callbacks::list_keys_callback)?;
    let delete_stored = helper_function(
        scope,
        "deleteStored",
        super::callbacks::delete_stored_callback,
    )?;
    let notify = helper_function(scope, "notify", notify_callback)?;
    let yield_control = helper_function(scope, "yield_control", yield_control_callback)?;
    let exit = helper_function(scope, "exit", exit_callback)?;

    set_global(scope, global, "tools", tools.into())?;
    let all_tools_key = v8::String::new(scope, "ALL_TOOLS")
        .ok_or_else(|| "failed to allocate ALL_TOOLS key".to_string())?;
    if global.set_lazy_data_property(scope, all_tools_key.into(), all_tools_callback) != Some(true)
    {
        return Err("failed to install lazy ALL_TOOLS".to_string());
    }
    set_global(scope, global, "ALL_TOOL_NAMES", all_tool_names)?;
    set_global(scope, global, "resolve_tool", resolve_tool.into())?;
    set_global(scope, global, "clearTimeout", clear_timeout.into())?;
    set_global(scope, global, "setTimeout", set_timeout.into())?;
    set_global(scope, global, "text", text.into())?;
    set_global(
        scope,
        global,
        "format_tool_result",
        format_tool_result.into(),
    )?;
    set_global(scope, global, "image", image.into())?;
    set_global(scope, global, "generatedImage", generated_image.into())?;
    set_global(scope, global, "store", store.into())?;
    set_global(scope, global, "load", load.into())?;
    set_global(scope, global, "listKeys", list_keys.into())?;
    set_global(scope, global, "deleteStored", delete_stored.into())?;
    set_global(scope, global, "notify", notify.into())?;
    set_global(scope, global, "yield_control", yield_control.into())?;
    set_global(scope, global, "exit", exit.into())?;
    let monotonic_now = helper_function(
        scope,
        "__codex_monotonic_now",
        super::callbacks::monotonic_now_callback,
    )?;
    set_global(scope, global, "__codex_monotonic_now", monotonic_now.into())?;
    let is_proxy = helper_function(
        scope,
        "__codex_is_proxy",
        super::callbacks::is_proxy_callback,
    )?;
    set_global(scope, global, "__codex_is_proxy", is_proxy.into())?;
    install_tool_aliases(scope, global, &enabled_tools)?;
    install_console_shim(scope, global)?;
    let text_encoding = v8::String::new(scope, include_str!("text_encoding.js"))
        .ok_or_else(|| "failed to allocate text encoding helpers".to_string())?;
    v8::Script::compile(scope, text_encoding, None)
        .and_then(|script| script.run(scope))
        .ok_or_else(|| "failed to install text encoding helpers".to_string())?;
    let graph_source = v8::String::new(scope, include_str!("dependency_graph.js"))
        .ok_or_else(|| "failed to allocate dependency graph helper".to_string())?;
    v8::Script::compile(scope, graph_source, None)
        .and_then(|script| script.run(scope))
        .ok_or_else(|| "failed to install dependency graph helper".to_string())?;
    let orchestration = v8::String::new(scope, include_str!("orchestration.js"))
        .ok_or_else(|| "failed to allocate orchestration helpers".to_string())?;
    v8::Script::compile(scope, orchestration, None)
        .and_then(|script| script.run(scope))
        .ok_or_else(|| "failed to install orchestration helpers".to_string())?;
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
        if let Some(index) = exec_command_index {
            install_tool_property(scope, global, alias, index)?;
        } else {
            let function = helper_function(scope, alias, missing_exec_alias_callback)?;
            set_global(scope, global, alias, function.into())?;
        }
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
    catalog: &super::EnabledToolCatalog,
) -> Result<v8::Local<'s, v8::Value>, String> {
    let enabled_tools = &catalog.tools;
    let array = v8::Array::new(scope, enabled_tools.len() as i32);
    for index in 0..enabled_tools.len() {
        let name = v8::String::new(scope, catalog.presentation_name(index))
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
    let diagnostic = if args.length() > 1 && !args.get(1).is_undefined() {
        match super::value::v8_value_to_json(scope, args.get(1)) {
            Ok(Some(options)) if options.is_object() => options["diagnostic"] == true,
            _ => {
                throw_type_error(
                    scope,
                    "resolve_tool options must be { diagnostic?: boolean }",
                );
                return;
            }
        }
    } else {
        false
    };
    let requested_name = (args.length() > 0 && args.get(0).is_string())
        .then(|| args.get(0).to_rust_string_lossy(scope));
    let enabled_tools = scope
        .get_slot::<RuntimeState>()
        .map(|state| Arc::clone(&state.enabled_tools));
    let Some(index) = enabled_tools.as_deref().and_then(|enabled_tools| {
        requested_name
            .as_deref()
            .and_then(|name| enabled_tools.resolve_requested_name(name))
    }) else {
        if diagnostic {
            let value = enabled_tools
                .as_deref()
                .unwrap_or(&EnabledToolCatalog::default())
                .resolution_diagnostic(requested_name.as_deref());
            match super::value::json_to_v8(scope, &value) {
                Some(value) => retval.set(value),
                None => throw_type_error(scope, "failed to create resolution diagnostic"),
            }
            return;
        }
        retval.set(v8::undefined(scope).into());
        return;
    };
    match tool_function(scope, index) {
        Ok(function) => retval.set(function.into()),
        Err(error) => throw_type_error(scope, &error),
    }
}

fn build_tools_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    catalog: &EnabledToolCatalog,
) -> Result<v8::Local<'s, v8::Object>, String> {
    let tools = v8::Object::new(scope);
    let enabled_tools = &catalog.tools;

    for (tool_index, tool) in enabled_tools.iter().enumerate() {
        install_tool_property(scope, tools, &tool.global_name, tool_index)?;
        let presentation = scope.get_slot::<RuntimeState>()
            .map(|state| state.enabled_tools.presentation_name(tool_index).to_owned());
        if let Some(presentation) = presentation
            && !presentation.contains('.') && presentation != tool.global_name
        {
            install_tool_property(scope, tools, &presentation, tool_index)?;
        }
    }
    // The catalog also uses this admitted set when rewriting declarations.
    for ((namespace, member), index) in &catalog.namespace_aliases {
        // Namespace aliases never shadow a canonical top-level callable.
        let namespace_key = v8::String::new(scope, namespace)
            .ok_or_else(|| "failed to allocate tool namespace".to_string())?;
        let namespace = if tools.has_own_property(scope, namespace_key.into()) == Some(true) {
            let value = tools
                .get(scope, namespace_key.into())
                .ok_or_else(|| "failed to read tool namespace".to_string())?;
            if value.is_function() {
                continue;
            }
            value
                .to_object(scope)
                .ok_or_else(|| "failed to read tool namespace".to_string())?
        } else {
            let namespace = v8::Object::new(scope);
            let null = v8::null(scope);
            namespace.set_prototype(scope, null.into());
            tools.create_data_property(scope, namespace_key.into(), namespace.into());
            namespace
        };
        install_tool_property(scope, namespace, member, *index)?;
    }
    Ok(tools)
}

fn all_tools_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<v8::Name>,
    _args: v8::PropertyCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let enabled_tools = scope
        .get_slot::<RuntimeState>()
        .map(|state| Arc::clone(&state.enabled_tools))
        .unwrap_or_default();
    match build_all_tools_value(scope, &enabled_tools) {
        Ok(value) => retval.set(value),
        Err(error) => throw_type_error(scope, &error),
    }
}

fn shared_description<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    description: &str,
) -> Result<v8::Local<'s, v8::String>, String> {
    #[cfg(test)]
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        state.description_materializations += 1;
    }
    // Internalization shares description storage between callable metadata and
    // the optional discovery array within this cell's isolate.
    v8::String::new_from_utf8(
        scope,
        description.as_bytes(),
        v8::NewStringType::Internalized,
    )
    .ok_or_else(|| "failed to allocate tool description".to_string())
}

fn build_all_tools_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    catalog: &super::EnabledToolCatalog,
) -> Result<v8::Local<'s, v8::Value>, String> {
    let enabled_tools = &catalog.tools;
    let array = v8::Array::new(scope, enabled_tools.len() as i32);
    let name_key = v8::String::new(scope, "name")
        .ok_or_else(|| "failed to allocate ALL_TOOLS name key".to_string())?;
    let description_key = v8::String::new(scope, "description")
        .ok_or_else(|| "failed to allocate ALL_TOOLS description key".to_string())?;
    let guidance_key = v8::String::new(scope, "namespace_instructions")
        .ok_or_else(|| "failed to allocate namespace instructions key".to_string())?;
    let guidance_ref_key = v8::String::new(scope, "namespace_instructions_ref")
        .ok_or_else(|| "failed to allocate namespace instructions reference key".to_string())?;
    let mut first_guidance = std::collections::BTreeMap::new();

    for (index, tool) in enabled_tools.iter().enumerate() {
        let item = v8::Object::new(scope);
        let name = v8::String::new(scope, catalog.presentation_name(index))
            .ok_or_else(|| "failed to allocate ALL_TOOLS name".to_string())?;
        // This boundary is explicit producer metadata, never inferred shared
        // prose. Full callable metadata and resolve_tool remain self-contained.
        let guidance = tool.namespace_description.as_deref().map(str::trim).filter(|text| !text.is_empty());
        let local = guidance.and_then(|guidance| tool.description.strip_prefix(guidance))
            .and_then(|rest| rest.strip_prefix("\n\n")).unwrap_or(&tool.description);
        let description = shared_description(scope, local)?;
        if let Some(guidance) = guidance {
            let first = *first_guidance.entry((tool.tool_name.namespace.as_deref(), guidance)).or_insert(index);
            let reference = shared_description(scope, catalog.presentation_name(first))?;
            if item.set(scope, guidance_ref_key.into(), reference.into()) != Some(true) {
                return Err("failed to set namespace instructions reference".into());
            }
            if first == index {
                let instructions = shared_description(scope, guidance)?;
                if item.set(scope, guidance_key.into(), instructions.into()) != Some(true) {
                    return Err("failed to set namespace instructions".into());
                }
            }
        }

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

fn install_tool_property<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    object: v8::Local<'s, v8::Object>,
    name: &str,
    index: usize,
) -> Result<(), String> {
    let key = v8::String::new(scope, name)
        .ok_or_else(|| "failed to allocate tool name".to_string())?;
    let index = u32::try_from(index).map_err(|_| "tool callback index exceeds u32".to_string())?;
    let data = v8::Integer::new_from_unsigned(scope, index);
    if object.set_lazy_data_property_with_data(
        scope,
        key.into(),
        tool_property_callback,
        data.into(),
        v8::PropertyAttribute::NONE,
        v8::SideEffectType::HasSideEffect,
        v8::SideEffectType::HasSideEffect,
    ) != Some(true) {
        return Err("failed to install lazy tool callable".to_string());
    }
    Ok(())
}

fn tool_property_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<v8::Name>,
    args: v8::PropertyCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let Some(index) = args.data().uint32_value(scope) else {
        throw_type_error(scope, "missing tool index");
        return;
    };
    match tool_function(scope, index as usize) {
        Ok(function) => retval.set(function.into()),
        Err(error) => throw_type_error(scope, &error),
    }
}

fn tool_function<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tool_index: usize,
) -> Result<v8::Local<'s, v8::Function>, String> {
    // One callable/metadata pair per immutable catalog entry in this cell.
    // Keep it in host state, not the user-mutable `tools` object. A new cell
    // owns a new cache, so changed capabilities cannot reuse old functions.
    let cached = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.tool_functions.get(&tool_index).cloned());
    if let Some(cached) = cached {
        return Ok(v8::Local::new(scope, cached));
    }
    let enabled_tools = scope
        .get_slot::<RuntimeState>()
        .map(|state| Arc::clone(&state.enabled_tools))
        .ok_or_else(|| "missing enabled tools".to_string())?;
    enabled_tools
        .get(tool_index)
        .ok_or_else(|| "missing enabled tool".to_string())?;
    let index =
        u32::try_from(tool_index).map_err(|_| "tool callback index exceeds u32".to_string())?;
    let data = v8::Integer::new_from_unsigned(scope, index);
    let template = v8::FunctionTemplate::builder(tool_callback)
        .data(data.into())
        .build(scope);
    let function = template
        .get_function(scope)
        .ok_or_else(|| "failed to create tool function".to_string())?;
    let metadata = v8::Object::new(scope);
    let name = v8::String::new(scope, enabled_tools.presentation_name(tool_index))
        .ok_or_else(|| "failed to allocate tool name".to_string())?;
    function.set_name(name);
    set_global(scope, metadata, "name", name.into())?;
    let description_key = v8::String::new(scope, "description")
        .ok_or_else(|| "failed to allocate description key".to_string())?;
    for object in [metadata, function.into()] {
        if object.set_lazy_data_property_with_data(
            scope,
            description_key.into(),
            tool_description_callback,
            data.into(),
            v8::PropertyAttribute::NONE,
            v8::SideEffectType::HasSideEffect,
            v8::SideEffectType::HasSideEffect,
        ) != Some(true)
        {
            return Err("failed to install lazy tool description".into());
        }
    }
    // Keep JSON discovery receipts compatible while allowing direct invocation.
    let to_json = v8::FunctionTemplate::builder(tool_metadata_callback)
        .data(metadata.into())
        .build(scope)
        .get_function(scope)
        .ok_or_else(|| "failed to create tool metadata serializer".to_string())?;
    set_global(scope, function.into(), "toJSON", to_json.into())?;
    let retained = v8::Global::new(scope, function);
    scope
        .get_slot_mut::<RuntimeState>()
        .ok_or_else(|| "missing runtime state".to_string())?
        .tool_functions
        .insert(tool_index, retained);
    Ok(function)
}

fn tool_metadata_callback(
    _scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    retval.set(args.data());
}

fn tool_description_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<v8::Name>,
    args: v8::PropertyCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let index = args.data().uint32_value(scope).map(|index| index as usize);
    let catalog = scope
        .get_slot::<RuntimeState>()
        .map(|state| Arc::clone(&state.enabled_tools));
    let Some(tool) = catalog
        .as_ref()
        .and_then(|catalog| index.and_then(|index| catalog.get(index)))
    else {
        throw_type_error(scope, "missing tool description");
        return;
    };
    match shared_description(scope, &tool.description) {
        Ok(description) => retval.set(description.into()),
        Err(error) => throw_type_error(scope, &error),
    }
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
