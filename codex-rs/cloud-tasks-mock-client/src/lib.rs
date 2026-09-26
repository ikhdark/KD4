//! Fixed cloud-task fixtures for tests and local development. Apply operations
//! simulate success without modifying a repository; task creation is unsupported.

use chrono::Utc;
use codex_cloud_tasks_client::ApplyOutcome;
use codex_cloud_tasks_client::ApplyStatus;
use codex_cloud_tasks_client::AttemptStatus;
use codex_cloud_tasks_client::CloudBackend;
use codex_cloud_tasks_client::CloudBackendFuture;
use codex_cloud_tasks_client::CloudTaskError;
use codex_cloud_tasks_client::CreatedTask;
use codex_cloud_tasks_client::DiffSummary;
use codex_cloud_tasks_client::Result;
use codex_cloud_tasks_client::TaskId;
use codex_cloud_tasks_client::TaskListPage;
use codex_cloud_tasks_client::TaskStatus;
use codex_cloud_tasks_client::TaskSummary;
use codex_cloud_tasks_client::TaskText;
use codex_cloud_tasks_client::TurnAttempt;

#[derive(Clone, Default)]
pub struct MockClient;

impl MockClient {
    async fn list_tasks(
        &self,
        env: Option<&str>,
        limit: Option<i64>,
        cursor: Option<&str>,
    ) -> Result<TaskListPage> {
        // Slightly vary content by env to aid tests that rely on the mock
        let rows = match env {
            Some("env-A") => vec![("T-2000", "A: First", TaskStatus::Ready)],
            Some("env-B") => vec![
                ("T-3000", "B: One", TaskStatus::Ready),
                ("T-3001", "B: Two", TaskStatus::Pending),
            ],
            None => vec![
                ("T-1000", "Update README formatting", TaskStatus::Ready),
                ("T-1001", "Fix clippy warnings in core", TaskStatus::Pending),
                ("T-1002", "Add contributing guide", TaskStatus::Ready),
            ],
            Some(_) => return Err(CloudTaskError::Unimplemented("unknown mock environment")),
        };
        let environment_id = env.map(str::to_string);
        let environment_label = match env {
            Some("env-A") => Some("Env A".to_string()),
            Some("env-B") => Some("Env B".to_string()),
            Some(other) => Some(other.to_string()),
            None => Some("Global".to_string()),
        };
        let mut out = Vec::new();
        for (id_str, title, status) in rows {
            let id = TaskId(id_str.to_string());
            let diff = mock_diff_for(&id);
            let (a, d) = count_from_unified(&diff);
            let attempt_total = mock_sibling_turn_ids(&id).len() + 1;
            out.push(TaskSummary {
                id,
                title: title.to_string(),
                status,
                updated_at: Some(fixture_timestamp()),
                environment_id: environment_id.clone(),
                environment_label: environment_label.clone(),
                summary: DiffSummary {
                    files_changed: 1,
                    lines_added: a,
                    lines_removed: d,
                },
                is_review: false,
                attempt_total: Some(attempt_total),
            });
        }
        let limit = match limit {
            Some(value) if value > 0 => usize::try_from(value)
                .map_err(|_| CloudTaskError::Msg("mock limit is too large".into()))?,
            Some(_) => return Err(CloudTaskError::Msg("mock limit must be positive".into())),
            None => out.len(),
        };
        let start = match cursor {
            Some(value) => value
                .parse::<usize>()
                .map_err(|_| CloudTaskError::Msg("invalid mock cursor".into()))?,
            None => 0,
        };
        if start > out.len() {
            return Err(CloudTaskError::Msg("mock cursor is out of range".into()));
        }
        let end = start.saturating_add(limit).min(out.len());
        let cursor = (end < out.len()).then(|| end.to_string());
        Ok(TaskListPage {
            tasks: out.into_iter().skip(start).take(end - start).collect(),
            cursor,
        })
    }

    async fn get_task_summary(&self, id: TaskId) -> Result<TaskSummary> {
        for env in [None, Some("env-A"), Some("env-B")] {
            if let Some(task) = self
                .list_tasks(env, None, None)
                .await?
                .tasks
                .into_iter()
                .find(|task| task.id == id)
            {
                return Ok(task);
            }
        }
        Err(CloudTaskError::Msg(format!(
            "Task {} not found (mock)",
            id.0
        )))
    }

    async fn get_task_diff(&self, id: TaskId) -> Result<Option<String>> {
        self.get_task_summary(id.clone()).await?;
        Ok(Some(mock_diff_for(&id)))
    }

    async fn get_task_text_and_diff(&self, id: TaskId) -> Result<(TaskText, Option<String>)> {
        self.get_task_summary(id.clone()).await?;
        let text = TaskText {
            prompt: Some("Review the fixture changes.".to_string()),
            messages: vec![
                "Mock assistant output: fixture changes are ready for review.".to_string(),
            ],
            turn_id: Some("mock-turn".to_string()),
            // Like the backend, name the other attempts wherever the list reports more than one.
            sibling_turn_ids: mock_sibling_turn_ids(&id),
            attempt_placement: Some(0),
            attempt_status: AttemptStatus::Completed,
        };
        Ok((text, Some(mock_diff_for(&id))))
    }

    async fn apply_task(&self, id: TaskId, diff_override: Option<String>) -> Result<ApplyOutcome> {
        self.validate_apply_input(&id, diff_override.as_deref())
            .await?;
        Ok(ApplyOutcome {
            applied: true,
            status: ApplyStatus::Success,
            message: format!("Simulated applying task {} (mock; no files changed)", id.0),
            skipped_paths: Vec::new(),
            conflict_paths: Vec::new(),
        })
    }

    async fn apply_task_preflight(
        &self,
        id: TaskId,
        diff_override: Option<String>,
    ) -> Result<ApplyOutcome> {
        self.validate_apply_input(&id, diff_override.as_deref())
            .await?;
        Ok(ApplyOutcome {
            applied: false,
            status: ApplyStatus::Success,
            message: format!("Preflight passed for task {} (mock)", id.0),
            skipped_paths: Vec::new(),
            conflict_paths: Vec::new(),
        })
    }

    async fn list_sibling_attempts(
        &self,
        task: TaskId,
        turn_id: String,
    ) -> Result<Vec<TurnAttempt>> {
        self.get_task_summary(task.clone()).await?;
        let sibling_turn_ids = mock_sibling_turn_ids(&task);
        if turn_id != "mock-turn" && !sibling_turn_ids.contains(&turn_id) {
            return Err(CloudTaskError::Unimplemented("unknown mock turn"));
        }
        Ok(sibling_turn_ids
            .into_iter()
            .enumerate()
            .map(|(index, turn_id)| TurnAttempt {
                turn_id,
                attempt_placement: i64::try_from(index + 1).ok(),
                created_at: Some(fixture_timestamp()),
                status: AttemptStatus::Completed,
                diff: Some(mock_diff_for(&task)),
                messages: vec!["Mock alternate attempt".to_string()],
            })
            .collect())
    }

    async fn validate_apply_input(&self, id: &TaskId, diff: Option<&str>) -> Result<()> {
        self.get_task_summary(id.clone()).await?;
        if diff.is_some_and(|diff| diff != mock_diff_for(id)) {
            return Err(CloudTaskError::Unimplemented(
                "custom mock diffs cannot be applied",
            ));
        }
        Ok(())
    }

    async fn create_task(
        &self,
        env_id: &str,
        prompt: &str,
        git_ref: &str,
        qa_mode: bool,
        best_of_n: usize,
    ) -> Result<CreatedTask> {
        let _ = (env_id, prompt, git_ref, qa_mode, best_of_n);
        Err(CloudTaskError::Unimplemented(
            "fixed mock tasks cannot be created",
        ))
    }
}

impl CloudBackend for MockClient {
    fn list_tasks<'a>(
        &'a self,
        env: Option<&'a str>,
        limit: Option<i64>,
        cursor: Option<&'a str>,
    ) -> CloudBackendFuture<'a, TaskListPage> {
        Box::pin(MockClient::list_tasks(self, env, limit, cursor))
    }

    fn get_task_summary(&self, id: TaskId) -> CloudBackendFuture<'_, TaskSummary> {
        Box::pin(MockClient::get_task_summary(self, id))
    }

    fn get_task_diff(&self, id: TaskId) -> CloudBackendFuture<'_, Option<String>> {
        Box::pin(MockClient::get_task_diff(self, id))
    }

    fn get_task_text_and_diff(
        &self,
        id: TaskId,
    ) -> CloudBackendFuture<'_, (TaskText, Option<String>)> {
        Box::pin(MockClient::get_task_text_and_diff(self, id))
    }

    fn apply_task(
        &self,
        id: TaskId,
        diff_override: Option<String>,
    ) -> CloudBackendFuture<'_, ApplyOutcome> {
        Box::pin(MockClient::apply_task(self, id, diff_override))
    }

    fn apply_task_preflight(
        &self,
        id: TaskId,
        diff_override: Option<String>,
    ) -> CloudBackendFuture<'_, ApplyOutcome> {
        Box::pin(MockClient::apply_task_preflight(self, id, diff_override))
    }

    fn list_sibling_attempts(
        &self,
        task: TaskId,
        turn_id: String,
    ) -> CloudBackendFuture<'_, Vec<TurnAttempt>> {
        Box::pin(MockClient::list_sibling_attempts(self, task, turn_id))
    }

    fn create_task<'a>(
        &'a self,
        env_id: &'a str,
        prompt: &'a str,
        git_ref: &'a str,
        qa_mode: bool,
        best_of_n: usize,
    ) -> CloudBackendFuture<'a, CreatedTask> {
        Box::pin(MockClient::create_task(
            self, env_id, prompt, git_ref, qa_mode, best_of_n,
        ))
    }
}

/// Turn ids of a fixture's other best-of-N attempts; the base attempt is always `mock-turn`.
fn mock_sibling_turn_ids(id: &TaskId) -> Vec<String> {
    match id.0.as_str() {
        "T-1000" => vec!["T-1000-attempt-2".to_string()],
        _ => Vec::new(),
    }
}

fn mock_diff_for(id: &TaskId) -> String {
    match id.0.as_str() {
        "T-1000" => {
            "diff --git a/README.md b/README.md\nindex 000000..111111 100644\n--- a/README.md\n+++ b/README.md\n@@ -1,2 +1,3 @@\n Intro\n-Hello\n+Hello, world!\n+Task: T-1000\n".to_string()
        }
        "T-1001" => {
            "diff --git a/core/src/lib.rs b/core/src/lib.rs\nindex 000000..111111 100644\n--- a/core/src/lib.rs\n+++ b/core/src/lib.rs\n@@ -1,2 +1,1 @@\n-use foo;\n use bar;\n".to_string()
        }
        _ => {
            "diff --git a/CONTRIBUTING.md b/CONTRIBUTING.md\nindex 000000..111111 100644\n--- /dev/null\n+++ b/CONTRIBUTING.md\n@@ -0,0 +1,3 @@\n+## Contributing\n+Please open PRs.\n+Thanks!\n".to_string()
        }
    }
}

fn count_from_unified(diff: &str) -> (usize, usize) {
    if let Ok(patch) = diffy::Patch::from_str(diff) {
        patch
            .hunks()
            .iter()
            .flat_map(diffy::Hunk::lines)
            .fold((0, 0), |(a, d), l| match l {
                diffy::Line::Insert(_) => (a + 1, d),
                diffy::Line::Delete(_) => (a, d + 1),
                _ => (a, d),
            })
    } else {
        let mut a = 0;
        let mut d = 0;
        for l in diff.lines() {
            if l.starts_with("+++") || l.starts_with("---") || l.starts_with("@@") {
                continue;
            }
            match l.as_bytes().first() {
                Some(b'+') => a += 1,
                Some(b'-') => d += 1,
                _ => {}
            }
        }
        (a, d)
    }
}

#[expect(
    clippy::expect_used,
    reason = "This fixed fixture timestamp is within the supported date range"
)]
fn fixture_timestamp() -> chrono::DateTime<Utc> {
    chrono::DateTime::from_timestamp(1_735_689_600, 0).expect("valid fixture timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn listed_fixtures_are_retrievable_and_stable() {
        let backend: &dyn CloudBackend = &MockClient;
        for env in [None, Some("env-A"), Some("env-B")] {
            let page = backend.list_tasks(env, None, None).await.expect("list");
            assert_eq!(
                page.tasks.len(),
                match env {
                    None => 3,
                    Some("env-A") => 1,
                    _ => 2,
                }
            );
            for task in page.tasks {
                assert_eq!(
                    backend
                        .get_task_summary(task.id.clone())
                        .await
                        .expect("summary"),
                    task
                );
                assert_eq!(
                    task.updated_at.expect("fixture timestamp").timestamp(),
                    1_735_689_600
                );
                assert!(
                    backend
                        .get_task_diff(task.id.clone())
                        .await
                        .expect("diff")
                        .is_some()
                );
                // Clients discover best-of-N attempts only through `sibling_turn_ids`, so it
                // must agree with the listed attempt count and the sibling attempts served.
                let (text, _) = backend
                    .get_task_text_and_diff(task.id.clone())
                    .await
                    .expect("task text");
                assert_eq!(
                    task.attempt_total,
                    Some(text.sibling_turn_ids.len() + 1),
                    "{}",
                    task.id.0
                );
                let turn_id = text.turn_id.expect("base attempt turn");
                let siblings = backend
                    .list_sibling_attempts(task.id.clone(), turn_id)
                    .await
                    .expect("siblings");
                assert_eq!(
                    siblings
                        .into_iter()
                        .map(|attempt| attempt.turn_id)
                        .collect::<Vec<_>>(),
                    text.sibling_turn_ids,
                    "{}",
                    task.id.0
                );
            }
        }
        let siblings = backend
            .list_sibling_attempts(TaskId("T-1000".into()), "mock-turn".into())
            .await
            .expect("siblings");
        assert_eq!(siblings.len(), 1);
        assert_eq!(
            siblings[0].created_at.expect("timestamp").timestamp(),
            1_735_689_600
        );
    }

    #[tokio::test]
    async fn pagination_returns_each_fixture_once() {
        let backend: &dyn CloudBackend = &MockClient;
        let mut cursor = None;
        for (index, expected) in ["T-1000", "T-1001", "T-1002"].into_iter().enumerate() {
            let page = backend
                .list_tasks(None, Some(1), cursor.as_deref())
                .await
                .expect("page");
            assert_eq!(page.tasks.len(), 1);
            assert_eq!(page.tasks[0].id.0, expected);
            assert_eq!(page.cursor, (index < 2).then(|| (index + 1).to_string()));
            cursor = page.cursor;
        }
        assert!(backend.list_tasks(None, Some(0), None).await.is_err());
        assert!(backend.list_tasks(None, None, Some("bad")).await.is_err());
        assert!(backend.list_tasks(None, None, Some("99")).await.is_err());
        assert!(
            backend
                .list_tasks(Some("unknown"), None, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unsupported_operations_and_unknown_ids_are_rejected() {
        let backend: &dyn CloudBackend = &MockClient;
        let id = TaskId("unknown".into());
        assert!(backend.get_task_summary(id.clone()).await.is_err());
        assert!(backend.get_task_diff(id.clone()).await.is_err());
        assert!(backend.get_task_text_and_diff(id.clone()).await.is_err());
        assert!(
            backend
                .list_sibling_attempts(id.clone(), "mock-turn".into())
                .await
                .is_err()
        );
        assert!(backend.apply_task(id.clone(), None).await.is_err());
        assert!(backend.apply_task_preflight(id, None).await.is_err());
        assert!(matches!(
            backend.create_task("env-A", "test", "main", false, 1).await,
            Err(CloudTaskError::Unimplemented(_))
        ));
        let id = TaskId("T-1000".into());
        assert!(
            backend
                .apply_task(id.clone(), Some("custom".into()))
                .await
                .is_err()
        );
        assert!(
            backend
                .apply_task_preflight(id.clone(), Some("custom".into()))
                .await
                .is_err()
        );
        assert!(
            backend
                .list_sibling_attempts(id.clone(), "unknown".into())
                .await
                .is_err()
        );
        let diff = backend
            .get_task_diff(id.clone())
            .await
            .expect("fixture diff");
        let preflight = backend
            .apply_task_preflight(id.clone(), diff.clone())
            .await
            .expect("preflight");
        assert_eq!(preflight.status, ApplyStatus::Success);
        assert!(!preflight.applied);
        let applied = backend.apply_task(id, diff).await.expect("simulated apply");
        assert_eq!(applied.status, ApplyStatus::Success);
        assert!(applied.applied);
        assert!(applied.message.contains("no files changed"));
    }
}
