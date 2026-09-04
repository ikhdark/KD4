use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::Hook;
use crate::HookEvent;
use crate::HookPayload;
use crate::HookResult;
use crate::command_from_argv;
use crate::engine::command_runner::run_contained_command;

const LEGACY_NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);
const MUTATING_FINALIZER_TIMEOUT: Duration = Duration::from_secs(10);

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

                match run_contained_command(command, Some(LEGACY_NOTIFY_TIMEOUT)).await {
                    Ok(status) if status.success() => HookResult::Success,
                    Ok(status) => HookResult::FailedContinue(
                        std::io::Error::other(format!("legacy notify exited with status {status}"))
                            .into(),
                    ),
                    Err(err) => HookResult::FailedContinue(err.into()),
                }
            })
        }),
    }
}

pub fn mutating_finalizer_hook(argv: Vec<String>) -> Hook {
    mutating_finalizer_hook_with_timeout(argv, MUTATING_FINALIZER_TIMEOUT)
}

fn mutating_finalizer_hook_with_timeout(argv: Vec<String>, execution_timeout: Duration) -> Hook {
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

                match run_contained_command(command, Some(execution_timeout)).await {
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

    fn redirected_descendant_command(
        directory: &std::path::Path,
        keep_root_alive: bool,
        descendant_delay_seconds: u64,
    ) -> (
        Vec<String>,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let started = directory.join("descendant-started.txt");
        let escaped = directory.join("descendant-escaped.txt");
        let direct_finished = directory.join("direct-finished.txt");

        #[cfg(windows)]
        let argv = {
            let child_script = directory.join("redirected-descendant.ps1");
            let root_script = directory.join("legacy-hook-root.ps1");
            let stdout = directory.join("redirected-descendant.stdout");
            let stderr = directory.join("redirected-descendant.stderr");
            let quote = |path: &std::path::Path| path.to_string_lossy().replace('\'', "''");
            std::fs::write(
                &child_script,
                format!(
                    "Set-Content -LiteralPath '{}' -Value started\nStart-Sleep -Seconds {descendant_delay_seconds}\nSet-Content -LiteralPath '{}' -Value escaped\n",
                    quote(&started),
                    quote(&escaped),
                ),
            )
            .expect("write redirected descendant script");
            std::fs::write(
                &root_script,
                format!(
                    "$null = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile', '-File', '{}') -WindowStyle Hidden -RedirectStandardOutput '{}' -RedirectStandardError '{}'; while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 10 }}\n{}\n",
                    quote(&child_script),
                    quote(&stdout),
                    quote(&stderr),
                    quote(&started),
                    if keep_root_alive {
                        "Start-Sleep -Seconds 60".to_string()
                    } else {
                        format!(
                            "Start-Sleep -Milliseconds 250; Set-Content -LiteralPath '{}' -Value finished",
                            quote(&direct_finished),
                        )
                    },
                ),
            )
            .expect("write legacy hook root script");
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-File".to_string(),
                root_script.to_string_lossy().into_owned(),
            ]
        };
        #[cfg(not(windows))]
        let argv = {
            let stdout = directory.join("redirected-descendant.stdout");
            let stderr = directory.join("redirected-descendant.stderr");
            let quote = |path: &std::path::Path| path.to_string_lossy().replace('\'', "'\\''");
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    "(printf started > '{}'; sleep {descendant_delay_seconds}; printf escaped > '{}') </dev/null > '{}' 2> '{}' & while [ ! -f '{}' ]; do sleep 0.01; done; {}",
                    quote(&started),
                    quote(&escaped),
                    quote(&stdout),
                    quote(&stderr),
                    quote(&started),
                    if keep_root_alive {
                        "sleep 60".to_string()
                    } else {
                        format!(
                            "sleep 0.25; printf finished > '{}'",
                            quote(&direct_finished),
                        )
                    },
                ),
                "codex-hook-test".to_string(),
            ]
        };

        (argv, started, escaped, direct_finished)
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
    async fn legacy_notify_waits_for_root_and_terminates_redirected_descendants() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (argv, started, escaped, direct_finished) = redirected_descendant_command(
            temp_dir.path(),
            /*keep_root_alive*/ false,
            /*descendant_delay_seconds*/ 2,
        );
        let hook = notify_hook(argv);
        let payload = after_agent_payload(temp_dir.path());

        let response = hook.execute(&payload).await;

        assert!(matches!(response.result, HookResult::Success));
        assert!(
            direct_finished.exists(),
            "legacy notify returned before its root process finished"
        );
        assert!(
            started.exists(),
            "the redirected descendant did not actually start"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !escaped.exists(),
            "a redirected descendant survived successful legacy notify completion"
        );
    }

    #[tokio::test]
    async fn cancelling_mutating_finalizer_terminates_redirected_descendants() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (argv, started, escaped, _direct_finished) = redirected_descendant_command(
            temp_dir.path(),
            /*keep_root_alive*/ true,
            /*descendant_delay_seconds*/ 2,
        );
        let hook = mutating_finalizer_hook(argv);
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
            "a redirected mutating-finalizer descendant survived cancellation"
        );
    }

    #[tokio::test]
    async fn mutating_finalizer_timeout_terminates_redirected_descendants() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let test_timeout = Duration::from_secs(8);
        let descendant_delay = Duration::from_secs(10);
        let (argv, started, escaped, _direct_finished) = redirected_descendant_command(
            temp_dir.path(),
            /*keep_root_alive*/ true,
            descendant_delay.as_secs(),
        );
        let hook = mutating_finalizer_hook_with_timeout(argv, test_timeout);
        let payload = after_agent_payload(temp_dir.path());

        let response = tokio::time::timeout(
            test_timeout + Duration::from_secs(5),
            hook.execute(&payload),
        )
        .await
        .expect("mutating finalizer should enforce its execution deadline");

        match response.result {
            HookResult::FailedAbort(error) => assert!(
                error
                    .to_string()
                    .contains(&format!("hook timed out after {}s", test_timeout.as_secs())),
                "unexpected finalizer timeout error: {error}"
            ),
            result => panic!("timed-out mutating finalizer should abort, got {result:?}"),
        }
        assert!(started.exists(), "the redirected descendant did not start");
        tokio::time::sleep(descendant_delay + Duration::from_secs(1)).await;
        assert!(
            !escaped.exists(),
            "a redirected mutating-finalizer descendant survived its execution timeout"
        );
    }
}
