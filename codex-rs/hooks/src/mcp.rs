use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use regex::Regex;
use serde_json::{Map, Value};
use tokio::time::Instant as TokioInstant;

use crate::engine::ConfiguredHandler;
use crate::engine::command_runner::CommandRunResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpHookTarget {
    pub server: String,
    pub tool: String,
    pub input: Map<String, Value>,
}

pub struct HookMcpCall {
    pub server: String,
    pub tool: String,
    pub input: Map<String, Value>,
    /// Absolute wall-clock limit, shared with any plugin trust verification.
    pub wall_deadline: TokioInstant,
}

/// Host adapter into the already-authorized, session-scoped MCP runtime.
/// The adapter must enforce the supplied wall deadline while it owns the call.
pub trait HookMcpExecutor: Send + Sync {
    fn execute(&self, call: HookMcpCall) -> BoxFuture<'_, Result<String>>;
}

pub(crate) async fn run_mcp_tool(
    executor: Option<&dyn HookMcpExecutor>,
    handler: &ConfiguredHandler,
    target: &McpHookTarget,
    event_json: &str,
) -> CommandRunResult {
    let started_at = chrono::Utc::now().timestamp();
    let started = Instant::now();
    let result = async {
        let executor = executor.context("MCP hook executor is unavailable")?;
        let event: Value = serde_json::from_str(event_json)?;
        let input = expand_object(&target.input, &event)?;
        // Let Tokio choose the representable cutoff, including for very large
        // configured durations. Trust and the call consume this same budget.
        let wall_deadline = tokio::time::sleep(Duration::from_secs(handler.timeout_sec)).deadline();
        if let Some(trust) = &handler.plugin_trust {
            tokio::time::timeout_at(wall_deadline, trust.verify())
                .await
                .context("hook trust verification timed out")??;
            if TokioInstant::now() >= wall_deadline {
                bail!("hook trust verification timed out");
            }
        }
        // This shares the existing effects ledger; timing out is not rollback.
        crate::mark_command_effects_possible();
        let text = executor.execute(HookMcpCall {
            server: target.server.clone(), tool: target.tool.clone(), input, wall_deadline,
        }).await?;
        if text.len() > 1024 * 1024 {
            bail!("MCP hook output exceeds the 1 MiB hook capture bound");
        }
        Ok::<_, anyhow::Error>(text)
    }.await;
    let (exit_code, stdout, error) = match result {
        Ok(text) => (Some(0), text, None),
        Err(error) => (None, String::new(), Some(error.to_string())),
    };
    CommandRunResult {
        started_at, completed_at: chrono::Utc::now().timestamp(),
        duration_ms: started.elapsed().as_millis().try_into().unwrap_or(i64::MAX),
        exit_code, stdout, stderr: String::new(), error,
    }
}

fn expand_object(input: &Map<String, Value>, event: &Value) -> Result<Map<String, Value>> {
    input.iter().map(|(key, value)| Ok((key.clone(), expand_value(value, event)?))).collect()
}

fn expand_value(value: &Value, event: &Value) -> Result<Value> {
    match value {
        Value::Object(object) => Ok(Value::Object(expand_object(object, event)?)),
        Value::Array(items) => items.iter().map(|item| expand_value(item, event))
            .collect::<Result<Vec<_>>>().map(Value::Array),
        Value::String(text) => expand_string(text, event),
        _ => Ok(value.clone()),
    }
}

fn expand_string(text: &str, event: &Value) -> Result<Value> {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\$\{([^{}]+)\}").unwrap_or_else(|_| unreachable!("static hook placeholder pattern"))
    });
    let captures = PATTERN.captures_iter(text).collect::<Vec<_>>();
    if captures.is_empty() { return Ok(Value::String(text.to_string())); }
    if captures.len() == 1 && captures[0].get(0).is_some_and(|m| m.start() == 0 && m.end() == text.len()) {
        return resolve_path(event, &captures[0][1]).cloned();
    }
    let mut result = String::new();
    let mut end = 0;
    for capture in captures {
        let Some(found) = capture.get(0) else { continue };
        result.push_str(&text[end..found.start()]);
        let value = resolve_path(event, &capture[1])?;
        match value {
            Value::String(text) => result.push_str(text),
            _ => result.push_str(&serde_json::to_string(value)?),
        }
        end = found.end();
    }
    result.push_str(&text[end..]);
    Ok(Value::String(result))
}

fn resolve_path<'a>(event: &'a Value, path: &str) -> Result<&'a Value> {
    path.split('.').try_fold(event, |value, field| {
        value.get(field).with_context(|| format!("hook input placeholder `${{{path}}}` was not found"))
    })
}
