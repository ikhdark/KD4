use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arc_swap::ArcSwapOption;
use codex_hooks::{HookMcpCall, HookMcpExecutor};
use codex_protocol::ThreadId;
use futures::{FutureExt, future::BoxFuture};
use serde_json::Value;

use crate::session::McpRuntimeSnapshot;

pub(crate) struct CoreHookMcpExecutor {
    pub(crate) runtime: Arc<ArcSwapOption<McpRuntimeSnapshot>>,
    pub(crate) thread_id: ThreadId,
}

impl HookMcpExecutor for CoreHookMcpExecutor {
    fn execute(&self, call: HookMcpCall) -> BoxFuture<'_, Result<String>> {
        async move {
            // Pin one published generation without holding a session/state lock.
            // The manager retains its configured server and per-tool deny filters.
            let runtime = self.runtime.load_full().context("MCP runtime is unavailable")?;
            let result = tokio::time::timeout(call.timeout, runtime.manager().call_tool(
                &call.server, &call.tool, Some(Value::Object(call.input)),
                Some(serde_json::json!({"threadId": self.thread_id.to_string()})),
            )).await.context("MCP hook timed out")??;
            let mut text = String::new();
            for content in &result.content {
                if content.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(part) = content.get("text").and_then(Value::as_str)
                {
                    if text.len().saturating_add(part.len()).saturating_add(1) > 1024 * 1024 {
                        bail!("MCP hook output exceeds the 1 MiB hook capture bound");
                    }
                    if !text.is_empty() { text.push('\n'); }
                    text.push_str(part);
                }
            }
            if result.is_error == Some(true) { bail!("MCP tool returned an error: {text}"); }
            Ok(text)
        }.boxed()
    }
}
