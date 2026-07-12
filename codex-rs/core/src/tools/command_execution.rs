use std::collections::HashMap;
use std::collections::VecDeque;
use std::hash::Hash;
use std::hash::Hasher;

use tokio::sync::Mutex;

use crate::tools::command_output_artifact::RawOutputArtifact;

const MAX_TRACKED_COMMANDS: usize = 128;
const MAX_TRACKED_PROCESSES: usize = 64;
const MAX_CONSECUTIVE_FAILURES: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CommandAttemptKey {
    tool_name: String,
    environment_id: String,
    cwd: String,
    command: Vec<String>,
}

impl CommandAttemptKey {
    pub(crate) fn new(
        tool_name: &str,
        environment_id: &str,
        cwd: impl Into<String>,
        command: &[String],
    ) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            environment_id: environment_id.to_string(),
            cwd: cwd.into(),
            command: command.to_vec(),
        }
    }

    pub(crate) fn fingerprint(&self) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandAttemptBlocked {
    pub(crate) fingerprint: String,
    pub(crate) consecutive_failures: u8,
}

impl CommandAttemptBlocked {
    pub(crate) fn render_for_model(&self) -> String {
        format!(
            "Command blocked: fingerprint `{}` has failed {} consecutive times in this session. Inspect the retained raw-output artifacts or change the command instead of repeating it unchanged.",
            self.fingerprint, self.consecutive_failures
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AttemptEntry {
    attempts: u32,
    repairs: u32,
    consecutive_failures: u8,
    last_exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
pub(crate) struct RunningCommand {
    pub(crate) key: CommandAttemptKey,
    pub(crate) artifact: RawOutputArtifact,
    completed_exit_code: Option<i32>,
}

#[derive(Default)]
struct CommandExecutionState {
    attempts: HashMap<CommandAttemptKey, AttemptEntry>,
    insertion_order: VecDeque<CommandAttemptKey>,
    running: HashMap<i32, RunningCommand>,
    running_order: VecDeque<i32>,
}

#[derive(Default)]
pub(crate) struct CommandExecutionLedger {
    state: Mutex<CommandExecutionState>,
}

impl CommandExecutionLedger {
    pub(crate) async fn begin_attempt(
        &self,
        key: &CommandAttemptKey,
        repaired: bool,
    ) -> Result<(), CommandAttemptBlocked> {
        let mut state = self.state.lock().await;
        if let Some(entry) = state.attempts.get(key)
            && entry.consecutive_failures >= MAX_CONSECUTIVE_FAILURES
        {
            return Err(CommandAttemptBlocked {
                fingerprint: key.fingerprint(),
                consecutive_failures: entry.consecutive_failures,
            });
        }

        if !state.attempts.contains_key(key) {
            while state.attempts.len() >= MAX_TRACKED_COMMANDS {
                let Some(oldest) = state.insertion_order.pop_front() else {
                    break;
                };
                state.attempts.remove(&oldest);
            }
            state.insertion_order.push_back(key.clone());
        }
        let entry = state.attempts.entry(key.clone()).or_default();
        entry.attempts = entry.attempts.saturating_add(1);
        if repaired {
            entry.repairs = entry.repairs.saturating_add(1);
        }
        Ok(())
    }

    pub(crate) async fn record_exit(&self, key: &CommandAttemptKey, exit_code: i32) {
        let mut state = self.state.lock().await;
        record_exit_locked(&mut state, key, exit_code);
    }

    pub(crate) async fn track_running_process(
        &self,
        process_id: i32,
        key: CommandAttemptKey,
        artifact: RawOutputArtifact,
    ) {
        let mut state = self.state.lock().await;
        if state.running.remove(&process_id).is_some() {
            state.running_order.retain(|tracked| *tracked != process_id);
        }
        while state.running.len() >= MAX_TRACKED_PROCESSES {
            let Some(oldest) = state.running_order.pop_front() else {
                break;
            };
            state.running.remove(&oldest);
        }
        state.running_order.push_back(process_id);
        state.running.insert(
            process_id,
            RunningCommand {
                key,
                artifact,
                completed_exit_code: None,
            },
        );
    }

    pub(crate) async fn running_process(&self, process_id: i32) -> Option<RunningCommand> {
        self.state.lock().await.running.get(&process_id).cloned()
    }

    pub(crate) async fn update_running_artifact(
        &self,
        process_id: i32,
        artifact: RawOutputArtifact,
    ) {
        if let Some(running) = self.state.lock().await.running.get_mut(&process_id) {
            running.artifact = artifact;
        }
    }

    pub(crate) async fn mark_running_process_completed(
        &self,
        process_id: i32,
        exit_code: i32,
    ) -> bool {
        let mut state = self.state.lock().await;
        let Some(running) = state.running.get_mut(&process_id) else {
            return false;
        };
        if running.completed_exit_code.is_some() {
            return true;
        }
        running.completed_exit_code = Some(exit_code);
        let key = running.key.clone();
        record_exit_locked(&mut state, &key, exit_code);
        true
    }

    pub(crate) async fn finish_running_process(
        &self,
        process_id: i32,
        exit_code: Option<i32>,
    ) -> bool {
        let mut state = self.state.lock().await;
        let Some(running) = state.running.remove(&process_id) else {
            return false;
        };
        state.running_order.retain(|tracked| *tracked != process_id);
        if running.completed_exit_code.is_none()
            && let Some(exit_code) = exit_code
        {
            record_exit_locked(&mut state, &running.key, exit_code);
        }
        true
    }

    #[cfg(test)]
    async fn snapshot(&self, key: &CommandAttemptKey) -> Option<AttemptEntry> {
        self.state.lock().await.attempts.get(key).cloned()
    }

    #[cfg(test)]
    pub(crate) async fn consecutive_failures(&self, key: &CommandAttemptKey) -> u8 {
        self.snapshot(key)
            .await
            .map_or(0, |entry| entry.consecutive_failures)
    }
}

fn record_exit_locked(state: &mut CommandExecutionState, key: &CommandAttemptKey, exit_code: i32) {
    let entry = state.attempts.entry(key.clone()).or_default();
    entry.last_exit_code = Some(exit_code);
    if exit_code == 0 {
        entry.consecutive_failures = 0;
    } else {
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(command: &str) -> CommandAttemptKey {
        CommandAttemptKey::new("exec_command", "local", "C:/repo", &[command.to_string()])
    }

    #[tokio::test]
    async fn blocks_third_identical_attempt_after_two_failures() {
        let ledger = CommandExecutionLedger::default();
        let key = key("fails.exe");

        ledger
            .begin_attempt(&key, false)
            .await
            .expect("first attempt");
        ledger.record_exit(&key, 7).await;
        ledger
            .begin_attempt(&key, false)
            .await
            .expect("second attempt");
        ledger.record_exit(&key, 7).await;

        let blocked = ledger
            .begin_attempt(&key, false)
            .await
            .expect_err("third attempt should be blocked");
        assert_eq!(blocked.consecutive_failures, 2);
        assert_eq!(blocked.fingerprint, key.fingerprint());
    }

    #[tokio::test]
    async fn success_resets_consecutive_failure_guard_and_repairs_are_counted() {
        let ledger = CommandExecutionLedger::default();
        let key = key("rg.exe");

        ledger
            .begin_attempt(&key, true)
            .await
            .expect("repaired attempt");
        ledger.record_exit(&key, 2).await;
        ledger
            .begin_attempt(&key, false)
            .await
            .expect("second attempt");
        ledger.record_exit(&key, 0).await;
        ledger
            .begin_attempt(&key, false)
            .await
            .expect("success should reset guard");

        let snapshot = ledger.snapshot(&key).await.expect("tracked entry");
        assert_eq!(snapshot.attempts, 3);
        assert_eq!(snapshot.repairs, 1);
        assert_eq!(snapshot.consecutive_failures, 0);
        assert_eq!(snapshot.last_exit_code, Some(0));
    }

    #[tokio::test]
    async fn background_completion_and_poll_finalize_one_failure_only() {
        let ledger = CommandExecutionLedger::default();
        let key = key("background-failure.exe");
        ledger.begin_attempt(&key, false).await.expect("attempt");
        ledger
            .track_running_process(
                42,
                key.clone(),
                RawOutputArtifact::Failed {
                    message: "fixture".to_string(),
                },
            )
            .await;

        assert!(ledger.mark_running_process_completed(42, 7).await);
        assert!(ledger.mark_running_process_completed(42, 7).await);
        assert!(ledger.finish_running_process(42, Some(7)).await);

        let snapshot = ledger.snapshot(&key).await.expect("tracked entry");
        assert_eq!(snapshot.consecutive_failures, 1);
        ledger
            .begin_attempt(&key, false)
            .await
            .expect("one failure must not block the next attempt");
    }
}
