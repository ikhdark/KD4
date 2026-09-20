use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::flat_tool_name;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::unified_exec::ExecCommandArgs;
use codex_memories_read::memory_root;
use codex_memories_read::usage::MEMORIES_USAGE_METRIC;
use codex_memories_read::usage::memories_usage_kinds_from_command;
use codex_protocol::models::ShellCommandToolCallParams;

pub(crate) fn emit_metric_for_tool_read(invocation: &ToolInvocation, success: bool) {
    let Some(command) = shell_script_for_invocation(invocation) else {
        return;
    };
    let ToolPayload::Function { arguments } = &invocation.payload else {
        return;
    };
    let Ok(context) = serde_json::from_str::<MemoryReadContext>(arguments) else {
        return;
    };
    let Ok(Some(environment)) = resolve_tool_environment(
        &invocation.step_context.environments,
        context.environment_id.as_deref(),
    ) else {
        return;
    };
    // Memories live in the host's Codex home. A remote path with the same text
    // does not identify those files.
    if environment.environment.is_remote() {
        return;
    }
    let cwd = match context.workdir.as_deref().filter(|path| !path.is_empty()) {
        Some(path) => environment.cwd().join(path),
        None => Ok(environment.cwd().clone()),
    };
    let Some(cwd) = cwd.ok().and_then(|path| path.to_abs_path().ok()) else {
        return;
    };
    let memory_root = memory_root(&invocation.step_context.turn.config.codex_home);

    let success = if success { "true" } else { "false" };
    let tool_name = flat_tool_name(&invocation.tool_name);
    for kind in memories_usage_kinds_from_command(&command, &cwd, &memory_root) {
        invocation.step_context.turn.session_telemetry.counter(
            MEMORIES_USAGE_METRIC,
            /*inc*/ 1,
            &[
                ("kind", kind.as_tag()),
                ("tool", tool_name.as_ref()),
                ("success", success),
            ],
        );
    }
}

#[derive(serde::Deserialize)]
struct MemoryReadContext {
    environment_id: Option<String>,
    workdir: Option<String>,
}

fn shell_script_for_invocation(invocation: &ToolInvocation) -> Option<String> {
    let ToolPayload::Function { arguments } = &invocation.payload else {
        return None;
    };

    match (
        invocation.tool_name.namespace.as_deref(),
        invocation.tool_name.name.as_str(),
    ) {
        (None, "shell_command") => serde_json::from_str::<ShellCommandToolCallParams>(arguments)
            .ok()
            .and_then(|params| {
                CommandInvocation::from_parts(
                    "shell_command",
                    "command",
                    params.command.as_deref(),
                    params.kind.as_deref(),
                    params.program.as_deref(),
                    params.args.as_deref(),
                    params.script_body.as_deref(),
                )
                .ok()
                .map(|command| command.display_command())
            }),
        (None, "exec_command") => serde_json::from_str::<ExecCommandArgs>(arguments)
            .ok()
            .map(|params| params.command_invocation().display_command()),
        (Some(_), _) | (None, _) => None,
    }
}
