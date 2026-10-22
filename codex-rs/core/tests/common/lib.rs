#![allow(clippy::expect_used)]

use ctor::ctor;

use codex_core::CodexThread;
pub use codex_core::test_support::TestCodexResponsesRequestKind;
pub use codex_core::test_support::responses_metadata;
pub use core_test_support_base::*;

pub mod apps_test_server;
pub mod context_snapshot;
pub mod hooks;
pub mod login_port;
pub mod process;
mod session_helpers;
pub mod test_codex;
pub mod test_codex_exec;

pub use session_helpers::*;

#[ctor(unsafe)]
fn enable_deterministic_unified_exec_process_ids_for_tests() {
    codex_core::test_support::set_thread_manager_test_mode(/*enabled*/ true);
    codex_core::test_support::set_deterministic_process_ids(/*enabled*/ true);
}

/// Waits for a configured MCP server to finish startup and requires it to be ready.
pub async fn wait_for_mcp_server(codex: &CodexThread, server_name: &str) -> anyhow::Result<()> {
    use codex_protocol::protocol::EventMsg;

    // Wait for the startup summary regardless of outcome, then interpret the
    // requested server's ready, failed, or cancelled entry below.
    let summary = wait_for_event_match(codex, |event| {
        if let EventMsg::McpStartupComplete(summary) = event {
            Some(summary.clone())
        } else {
            None
        }
    })
    .await;
    if let Some(failure) = summary
        .failed
        .iter()
        .find(|failure| failure.server == server_name)
    {
        let error = &failure.error;
        anyhow::bail!("MCP server {server_name} failed to start: {error}");
    }
    if summary.cancelled.iter().any(|server| server == server_name) {
        anyhow::bail!("MCP server {server_name} startup was cancelled");
    }
    assert!(
        summary.ready.iter().any(|server| server == server_name),
        "expected MCP server {server_name} to be ready; startup summary: {summary:?}"
    );
    Ok(())
}

pub async fn submit_thread_settings(
    codex: &CodexThread,
    thread_settings: codex_protocol::protocol::ThreadSettingsOverrides,
) -> anyhow::Result<()> {
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::Op;
    use tokio::time::Duration;
    use tokio::time::Instant;
    use tokio::time::timeout_at;

    let submission_id = codex.submit(Op::ThreadSettings { thread_settings }).await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ev = timeout_at(deadline, codex.next_event())
            .await
            .expect("timeout waiting for thread settings update")
            .expect("stream ended unexpectedly");
        if ev.id == submission_id {
            match ev.msg {
                EventMsg::ThreadSettingsApplied(_) => return Ok(()),
                EventMsg::Error(err) => panic!("thread settings update failed: {}", err.message),
                other => panic!("unexpected thread settings update event: {other:?}"),
            }
        }
    }
}

pub fn format_with_current_shell(command: &str) -> Vec<String> {
    codex_core::shell::default_user_shell()
        .derive_exec_args(command, /*use_login_shell*/ true)
        .expect("default Windows shell must be executable")
}

pub fn format_with_current_shell_display(command: &str) -> String {
    let args = format_with_current_shell(command);
    codex_app_server_protocol::command_display_string(&args)
}

pub fn format_with_current_shell_non_login(command: &str) -> Vec<String> {
    codex_core::shell::default_user_shell()
        .derive_exec_args(command, /*use_login_shell*/ false)
        .expect("default Windows shell must be executable")
}

pub fn format_with_current_shell_display_non_login(command: &str) -> String {
    let args = format_with_current_shell_non_login(command);
    codex_app_server_protocol::command_display_string(&args)
}

#[cfg(test)]
mod tests {
    use super::session_helpers::event_before_deadline;
    use super::*;

    #[tokio::test]
    async fn default_config_keeps_multi_agent_v2_enabled() {
        let codex_home = tempfile::tempdir().expect("create test Codex home");
        let config = load_default_config_for_test(&codex_home).await;

        assert!(
            config
                .features
                .enabled(codex_features::Feature::MultiAgentV2)
        );
    }

    #[tokio::test]
    async fn event_waiter_honors_requested_deadline() {
        use codex_protocol::protocol::Event;
        use codex_protocol::protocol::EventMsg;
        use tokio::time::Duration;

        // Only the event source is substituted; the production wait/match loop runs.
        let mut observed = 0;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            event_before_deadline(
                Duration::from_millis(20),
                || async {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    Event {
                        id: "unrelated-operation".into(),
                        msg: EventMsg::ShutdownComplete,
                    }
                },
                |event| {
                    observed += 1;
                    event.id == "submitted-operation"
                        && matches!(event.msg, EventMsg::ShutdownComplete)
                },
            ),
        )
        .await
        .expect("unrelated events must not renew the total deadline");
        assert!(result.is_err());
        assert!(
            observed > 0,
            "the waiter must reject an event of the right kind from the wrong operation"
        );
    }
}
