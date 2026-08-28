use std::process::Stdio;
use std::sync::Arc;

use serde::Serialize;

use crate::Hook;
use crate::HookEvent;
use crate::HookPayload;
use crate::HookResult;
use crate::command_from_argv;

/// Legacy notify payload appended as the final argv argument for backward compatibility.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum UserNotification {
    #[serde(rename_all = "kebab-case")]
    AgentTurnComplete {
        thread_id: String,
        turn_id: String,
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client: Option<String>,
        input_messages: Vec<String>,
        last_assistant_message: Option<String>,
    },
}

pub fn legacy_notify_json(payload: &HookPayload) -> Result<String, serde_json::Error> {
    match &payload.hook_event {
        HookEvent::AfterAgent { event } => {
            serde_json::to_string(&UserNotification::AgentTurnComplete {
                thread_id: event.thread_id.to_string(),
                turn_id: event.turn_id.clone(),
                cwd: payload.cwd.display().to_string(),
                client: payload.client.clone(),
                input_messages: event.input_messages.clone(),
                last_assistant_message: event.last_assistant_message.clone(),
            })
        }
    }
}

pub fn notify_hook(argv: Vec<String>) -> Hook {
    let argv = Arc::new(argv);
    Hook {
        name: "legacy_notify".to_string(),
        func: Arc::new(move |payload: &HookPayload| {
            let argv = Arc::clone(&argv);
            Box::pin(async move {
                let mut command = match command_from_argv(&argv) {
                    Some(command) => command,
                    None => return HookResult::Success,
                };
                if let Ok(notify_payload) = legacy_notify_json(payload) {
                    command.arg(notify_payload);
                }

                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());

                match command.spawn() {
                    Ok(_) => HookResult::Success,
                    Err(err) => HookResult::FailedContinue(err.into()),
                }
            })
        }),
    }
}

pub fn mutating_finalizer_hook(argv: Vec<String>) -> Hook {
    let argv = Arc::new(argv);
    Hook {
        name: "legacy_notify".to_string(),
        func: Arc::new(move |payload: &HookPayload| {
            let argv = Arc::clone(&argv);
            Box::pin(async move {
                let mut command = match command_from_argv(&argv) {
                    Some(command) => command,
                    None => return HookResult::Success,
                };
                if let Ok(notify_payload) = legacy_notify_json(payload) {
                    command.arg(notify_payload);
                }

                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);

                match command.status().await {
                    Ok(status) if status.success() => HookResult::Success,
                    Ok(status) => HookResult::FailedAbort(
                        std::io::Error::other(format!(
                            "mutating finalizer exited with status {status}"
                        ))
                        .into(),
                    ),
                    Err(err) => HookResult::FailedAbort(err.into()),
                }
            })
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::Result;
    use codex_protocol::ThreadId;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;

    use super::*;
    use crate::HookEventAfterAgent;

    fn after_agent_payload(cwd: &std::path::Path) -> HookPayload {
        HookPayload {
            session_id: ThreadId::new(),
            cwd: cwd
                .to_path_buf()
                .try_into()
                .expect("temporary directory should be absolute"),
            client: None,
            triggered_at: chrono::Utc::now(),
            hook_event: HookEvent::AfterAgent {
                event: HookEventAfterAgent {
                    thread_id: ThreadId::new(),
                    turn_id: "turn-1".to_string(),
                    input_messages: Vec::new(),
                    last_assistant_message: None,
                },
            },
        }
    }

    fn delayed_marker_command(directory: &std::path::Path) -> Vec<String> {
        #[cfg(windows)]
        {
            let script = directory.join("delayed-marker.ps1");
            std::fs::write(
                &script,
                concat!(
                    "Set-Content -LiteralPath (Join-Path $PSScriptRoot 'started.txt') -Value started\n",
                    "Start-Sleep -Seconds 2\n",
                    "Set-Content -LiteralPath (Join-Path $PSScriptRoot 'escaped.txt') -Value escaped\n",
                ),
            )
            .expect("write finalizer test script");
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-File".to_string(),
                script.to_string_lossy().into_owned(),
            ]
        }

        #[cfg(not(windows))]
        {
            let script = directory.join("delayed-marker.sh");
            std::fs::write(
                &script,
                concat!(
                    "script_dir=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n",
                    "printf started > \"$script_dir/started.txt\"\n",
                    "sleep 2\n",
                    "printf escaped > \"$script_dir/escaped.txt\"\n",
                ),
            )
            .expect("write finalizer test script");
            vec!["sh".to_string(), script.to_string_lossy().into_owned()]
        }
    }

    fn expected_notification_json() -> Value {
        let cwd = test_path_buf("/Users/example/project");
        json!({
            "type": "agent-turn-complete",
            "thread-id": "b5f6c1c2-1111-2222-3333-444455556666",
            "turn-id": "12345",
            "cwd": cwd.display().to_string(),
            "client": "codex-tui",
            "input-messages": ["Rename `foo` to `bar` and update the callsites."],
            "last-assistant-message": "Rename complete and verified `cargo build` succeeds.",
        })
    }

    #[test]
    fn test_user_notification() -> Result<()> {
        let notification = UserNotification::AgentTurnComplete {
            thread_id: "b5f6c1c2-1111-2222-3333-444455556666".to_string(),
            turn_id: "12345".to_string(),
            cwd: test_path_buf("/Users/example/project")
                .display()
                .to_string(),
            client: Some("codex-tui".to_string()),
            input_messages: vec!["Rename `foo` to `bar` and update the callsites.".to_string()],
            last_assistant_message: Some(
                "Rename complete and verified `cargo build` succeeds.".to_string(),
            ),
        };
        let serialized = serde_json::to_string(&notification)?;
        let actual: Value = serde_json::from_str(&serialized)?;
        assert_eq!(actual, expected_notification_json());
        Ok(())
    }

    #[test]
    fn legacy_notify_json_matches_historical_wire_shape() -> Result<()> {
        let payload = HookPayload {
            session_id: ThreadId::new(),
            cwd: test_path_buf("/Users/example/project").abs(),
            client: Some("codex-tui".to_string()),
            triggered_at: chrono::Utc::now(),
            hook_event: HookEvent::AfterAgent {
                event: HookEventAfterAgent {
                    thread_id: ThreadId::from_string("b5f6c1c2-1111-2222-3333-444455556666")
                        .expect("valid thread id"),
                    turn_id: "12345".to_string(),
                    input_messages: vec![
                        "Rename `foo` to `bar` and update the callsites.".to_string(),
                    ],
                    last_assistant_message: Some(
                        "Rename complete and verified `cargo build` succeeds.".to_string(),
                    ),
                },
            },
        };

        let serialized = legacy_notify_json(&payload)?;
        let actual: Value = serde_json::from_str(&serialized)?;
        assert_eq!(actual, expected_notification_json());

        Ok(())
    }

    #[tokio::test]
    async fn cancelling_mutating_finalizer_terminates_its_subprocess() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let started = temp_dir.path().join("started.txt");
        let escaped = temp_dir.path().join("escaped.txt");
        let hook = mutating_finalizer_hook(delayed_marker_command(temp_dir.path()));
        let payload = after_agent_payload(temp_dir.path());

        let hook_task = tokio::spawn(async move { hook.execute(&payload).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("finalizer subprocess should start");

        hook_task.abort();
        assert!(
            hook_task
                .await
                .expect_err("aborted finalizer task should not complete")
                .is_cancelled()
        );

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !escaped.exists(),
            "mutating finalizer subprocess survived cancellation"
        );
    }
}
