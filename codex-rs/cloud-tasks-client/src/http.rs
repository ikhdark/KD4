use crate::ApplyOutcome;
use crate::ApplyStatus;
use crate::AttemptStatus;
use crate::CloudBackend;
use crate::CloudBackendFuture;
use crate::CloudTaskError;
use crate::DiffSummary;
use crate::Result;
use crate::TaskId;
use crate::TaskListPage;
use crate::TaskStatus;
use crate::TaskSummary;
use crate::TurnAttempt;
use crate::api::TaskText;
use crate::append_error_log;
use chrono::DateTime;
use chrono::Utc;

use codex_api::SharedAuthProvider;
use codex_backend_client as backend;
use codex_git_utils::ApplyGitRequest;
use codex_git_utils::apply_git_patch;

#[derive(Clone)]
pub struct HttpClient {
    pub base_url: String,
    backend: backend::Client,
}

impl HttpClient {
    pub fn new(
        base_url: impl Into<String>,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> Self {
        let base_url = base_url.into();
        let backend = backend::Client::new(base_url.clone(), http_client_factory);
        Self { base_url, backend }
    }

    pub fn with_user_agent(mut self, ua: impl Into<String>) -> Self {
        self.backend = self.backend.clone().with_user_agent(ua);
        self
    }

    pub fn with_auth_provider(mut self, auth: SharedAuthProvider) -> Self {
        self.backend = self.backend.clone().with_auth_provider(auth);
        self
    }

    pub fn with_chatgpt_account_id(mut self, account_id: impl Into<String>) -> Self {
        self.backend = self.backend.clone().with_chatgpt_account_id(account_id);
        self
    }

    fn tasks_api(&self) -> api::Tasks<'_> {
        api::Tasks::new(self)
    }

    fn attempts_api(&self) -> api::Attempts<'_> {
        api::Attempts::new(self)
    }

    fn apply_api(&self) -> api::Apply<'_> {
        api::Apply::new(self)
    }
}

impl CloudBackend for HttpClient {
    fn list_tasks<'a>(
        &'a self,
        env: Option<&'a str>,
        limit: Option<i64>,
        cursor: Option<&'a str>,
    ) -> CloudBackendFuture<'a, TaskListPage> {
        Box::pin(async move { self.tasks_api().list(env, limit, cursor).await })
    }

    fn get_task_summary(&self, id: TaskId) -> CloudBackendFuture<'_, TaskSummary> {
        Box::pin(async move { self.tasks_api().summary(id).await })
    }

    fn get_task_diff(&self, id: TaskId) -> CloudBackendFuture<'_, Option<String>> {
        Box::pin(async move { self.tasks_api().diff(id).await })
    }

    fn get_task_messages(&self, id: TaskId) -> CloudBackendFuture<'_, Vec<String>> {
        Box::pin(async move { self.tasks_api().messages(id).await })
    }

    fn get_task_text(&self, id: TaskId) -> CloudBackendFuture<'_, TaskText> {
        Box::pin(async move { self.tasks_api().task_text(id).await })
    }

    fn list_sibling_attempts(
        &self,
        task: TaskId,
        turn_id: String,
    ) -> CloudBackendFuture<'_, Vec<TurnAttempt>> {
        Box::pin(async move { self.attempts_api().list(task, turn_id).await })
    }

    fn apply_task(
        &self,
        id: TaskId,
        diff_override: Option<String>,
    ) -> CloudBackendFuture<'_, ApplyOutcome> {
        Box::pin(async move {
            self.apply_api()
                .run(id, diff_override, /*preflight*/ false)
                .await
        })
    }

    fn apply_task_preflight(
        &self,
        id: TaskId,
        diff_override: Option<String>,
    ) -> CloudBackendFuture<'_, ApplyOutcome> {
        Box::pin(async move {
            self.apply_api()
                .run(id, diff_override, /*preflight*/ true)
                .await
        })
    }

    fn create_task<'a>(
        &'a self,
        env_id: &'a str,
        prompt: &'a str,
        git_ref: &'a str,
        qa_mode: bool,
        best_of_n: usize,
    ) -> CloudBackendFuture<'a, crate::CreatedTask> {
        Box::pin(async move {
            self.tasks_api()
                .create(env_id, prompt, git_ref, qa_mode, best_of_n)
                .await
        })
    }
}

mod api {
    use super::*;
    use serde_json::Value;
    use std::cmp::Ordering;
    use std::collections::HashMap;

    pub(crate) struct Tasks<'a> {
        base_url: &'a str,
        backend: &'a backend::Client,
    }

    impl<'a> Tasks<'a> {
        pub(crate) fn new(client: &'a HttpClient) -> Self {
            Self {
                base_url: &client.base_url,
                backend: &client.backend,
            }
        }

        pub(crate) async fn list(
            &self,
            env: Option<&str>,
            limit: Option<i64>,
            cursor: Option<&str>,
        ) -> Result<TaskListPage> {
            let limit_i32 = limit.and_then(|lim| i32::try_from(lim).ok());
            let resp = self
                .backend
                .list_tasks(limit_i32, Some("current"), env, cursor)
                .await
                .map_err(|e| CloudTaskError::Http(format!("list_tasks failed: {e}")))?;

            let tasks: Vec<TaskSummary> = resp
                .items
                .into_iter()
                .map(map_task_list_item_to_summary)
                .collect();

            append_error_log(format!(
                "http.list_tasks: env={} limit={} cursor_in={} cursor_out={} items={}",
                env.unwrap_or("<all>"),
                limit_i32
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "<default>".to_string()),
                cursor.unwrap_or("<none>"),
                resp.cursor.as_deref().unwrap_or("<none>"),
                tasks.len()
            ));
            Ok(TaskListPage {
                tasks,
                cursor: resp.cursor,
            })
        }

        pub(crate) async fn summary(&self, id: TaskId) -> Result<TaskSummary> {
            let id_str = id.0.clone();
            let (details, body, ct) = self
                .details_with_body(&id.0)
                .await
                .map_err(|e| CloudTaskError::Http(format!("get_task_details failed: {e}")))?;
            let parsed: Value = serde_json::from_str(&body).map_err(|e| {
                CloudTaskError::Http(format!(
                    "Decode error for {}: {e}; content-type={ct}; body={body}",
                    id.0
                ))
            })?;
            let task_obj = parsed
                .get("task")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    CloudTaskError::Http(format!("Task metadata missing from details for {id_str}"))
                })?;
            let status_display = parsed
                .get("task_status_display")
                .or_else(|| task_obj.get("task_status_display"))
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<HashMap<String, Value>>()
                });
            let status = map_status(status_display.as_ref());
            let mut summary = diff_summary_from_status_display(status_display.as_ref());
            if summary.files_changed == 0
                && summary.lines_added == 0
                && summary.lines_removed == 0
                && let Some(diff) = details.unified_diff()
            {
                summary = diff_summary_from_diff(&diff);
            }
            let updated_at_raw = task_obj
                .get("updated_at")
                .and_then(Value::as_f64)
                .or_else(|| task_obj.get("created_at").and_then(Value::as_f64))
                .or_else(|| latest_turn_timestamp(status_display.as_ref()));
            let environment_id = task_obj
                .get("environment_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let environment_label = env_label_from_status_display(status_display.as_ref());
            let attempt_total = attempt_total_from_status_display(status_display.as_ref());
            let title = task_obj
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("<untitled>")
                .to_string();
            let is_review = task_obj
                .get("is_review")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(TaskSummary {
                id,
                title,
                status,
                updated_at: parse_updated_at(updated_at_raw.as_ref()),
                environment_id,
                environment_label,
                summary,
                is_review,
                attempt_total,
            })
        }

        pub(crate) async fn diff(&self, id: TaskId) -> Result<Option<String>> {
            let (details, body, ct) = self
                .details_with_body(&id.0)
                .await
                .map_err(|e| CloudTaskError::Http(format!("get_task_details failed: {e}")))?;
            if let Some(diff) = details.unified_diff() {
                return Ok(Some(diff));
            }
            let _ = (body, ct);
            Ok(None)
        }

        pub(crate) async fn messages(&self, id: TaskId) -> Result<Vec<String>> {
            let (details, body, ct) = self
                .details_with_body(&id.0)
                .await
                .map_err(|e| CloudTaskError::Http(format!("get_task_details failed: {e}")))?;

            let mut msgs = details.assistant_text_messages();
            if msgs.is_empty() {
                msgs.extend(extract_assistant_messages_from_body(&body));
            }
            if !msgs.is_empty() {
                return Ok(msgs);
            }
            if let Some(err) = details.assistant_error_message() {
                return Ok(vec![format!("Task failed: {err}")]);
            }

            let url = match details_path(self.base_url, &id.0) {
                Some(url) => url,
                None => format!("{}/api/codex/tasks/{}", self.base_url, id.0),
            };
            Err(CloudTaskError::Http(format!(
                "No assistant text messages in response. GET {url}; content-type={ct}; body={body}"
            )))
        }

        pub(crate) async fn task_text(&self, id: TaskId) -> Result<TaskText> {
            let (details, body, _ct) = self
                .details_with_body(&id.0)
                .await
                .map_err(|e| CloudTaskError::Http(format!("get_task_details failed: {e}")))?;
            let prompt = details.user_text_prompt();
            let mut messages = details.assistant_text_messages();
            if messages.is_empty() {
                messages.extend(extract_assistant_messages_from_body(&body));
            }
            let assistant_turn = details.current_assistant_turn.as_ref();
            let turn_id = assistant_turn.and_then(|turn| turn.id.clone());
            let sibling_turn_ids = assistant_turn
                .map(|turn| turn.sibling_turn_ids.clone())
                .unwrap_or_default();
            let attempt_placement = assistant_turn.and_then(|turn| turn.attempt_placement);
            let attempt_status = attempt_status_from_str(
                assistant_turn.and_then(|turn| turn.turn_status.as_deref()),
            );
            Ok(TaskText {
                prompt,
                messages,
                turn_id,
                sibling_turn_ids,
                attempt_placement,
                attempt_status,
            })
        }

        pub(crate) async fn create(
            &self,
            env_id: &str,
            prompt: &str,
            git_ref: &str,
            qa_mode: bool,
            best_of_n: usize,
        ) -> Result<crate::CreatedTask> {
            let mut input_items: Vec<serde_json::Value> = Vec::new();
            input_items.push(serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{ "content_type": "text", "text": prompt }]
            }));

            if let Ok(diff) = std::env::var("CODEX_STARTING_DIFF")
                && !diff.is_empty()
            {
                input_items.push(serde_json::json!({
                    "type": "pre_apply_patch",
                    "output_diff": { "diff": diff }
                }));
            }

            let mut request_body = serde_json::json!({
                "new_task": {
                    "environment_id": env_id,
                    "branch": git_ref,
                    "run_environment_in_qa_mode": qa_mode,
                },
                "input_items": input_items,
            });

            if best_of_n > 1
                && let Some(obj) = request_body.as_object_mut()
            {
                obj.insert(
                    "metadata".to_string(),
                    serde_json::json!({ "best_of_n": best_of_n }),
                );
            }

            match self.backend.create_task(request_body).await {
                Ok(id) => {
                    append_error_log(format!(
                        "new_task: created id={id} env={} prompt_chars={}",
                        env_id,
                        prompt.chars().count()
                    ));
                    Ok(crate::CreatedTask { id: TaskId(id) })
                }
                Err(e) => {
                    append_error_log(format!(
                        "new_task: create failed env={} prompt_chars={}: {}",
                        env_id,
                        prompt.chars().count(),
                        e
                    ));
                    Err(CloudTaskError::Http(format!("create_task failed: {e}")))
                }
            }
        }

        async fn details_with_body(
            &self,
            id: &str,
        ) -> anyhow::Result<(backend::CodeTaskDetailsResponse, String, String)> {
            let (parsed, body, ct) = self.backend.get_task_details_with_body(id).await?;
            Ok((parsed, body, ct))
        }
    }

    pub(crate) struct Attempts<'a> {
        backend: &'a backend::Client,
    }

    impl<'a> Attempts<'a> {
        pub(crate) fn new(client: &'a HttpClient) -> Self {
            Self {
                backend: &client.backend,
            }
        }

        pub(crate) async fn list(&self, task: TaskId, turn_id: String) -> Result<Vec<TurnAttempt>> {
            let resp = self
                .backend
                .list_sibling_turns(&task.0, &turn_id)
                .await
                .map_err(|e| CloudTaskError::Http(format!("list_sibling_turns failed: {e}")))?;

            let mut attempts: Vec<TurnAttempt> = resp
                .sibling_turns
                .iter()
                .filter_map(turn_attempt_from_map)
                .collect();
            attempts.sort_by(compare_attempts);
            Ok(attempts)
        }
    }

    pub(crate) struct Apply<'a> {
        backend: &'a backend::Client,
    }

    impl<'a> Apply<'a> {
        pub(crate) fn new(client: &'a HttpClient) -> Self {
            Self {
                backend: &client.backend,
            }
        }

        pub(crate) async fn run(
            &self,
            task_id: TaskId,
            diff_override: Option<String>,
            preflight: bool,
        ) -> Result<ApplyOutcome> {
            let id = task_id.0.clone();
            let diff = match diff_override {
                Some(diff) => diff,
                None => {
                    let details = self.backend.get_task_details(&id).await.map_err(|e| {
                        CloudTaskError::Http(format!("get_task_details failed: {e}"))
                    })?;
                    details.unified_diff().ok_or_else(|| {
                        CloudTaskError::Msg(format!("No diff available for task {id}"))
                    })?
                }
            };

            let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
            // The worker owns the request and Git's temporary files until the
            // entire application finishes, even if the awaiting caller leaves.
            tokio::task::spawn_blocking(move || Self::apply_diff(id, diff, preflight, cwd))
                .await
                .map_err(|e| CloudTaskError::Io(format!("git apply worker failed: {e}")))?
        }

        fn apply_diff(
            id: String,
            diff: String,
            preflight: bool,
            cwd: std::path::PathBuf,
        ) -> Result<ApplyOutcome> {
            if !is_unified_diff(&diff) {
                let summary = summarize_patch_for_logging(&diff);
                let mode = if preflight { "preflight" } else { "apply" };
                append_error_log(format!(
                    "apply_error: id={id} mode={mode} format=non-unified; {summary}"
                ));
                return Ok(ApplyOutcome {
                    applied: false,
                    status: ApplyStatus::Error,
                    message: "Expected unified git diff; backend returned an incompatible format."
                        .to_string(),
                    skipped_paths: Vec::new(),
                    conflict_paths: Vec::new(),
                });
            }

            let req = ApplyGitRequest {
                cwd,
                diff: diff.clone(),
                revert: false,
                preflight,
            };
            let r = apply_git_patch(&req)
                .map_err(|e| CloudTaskError::Io(format!("git apply failed to run: {e}")))?;

            let status = if r.exit_code == 0 {
                ApplyStatus::Success
            } else if !r.applied_paths.is_empty() || !r.conflicted_paths.is_empty() {
                ApplyStatus::Partial
            } else {
                ApplyStatus::Error
            };
            let applied = matches!(status, ApplyStatus::Success) && !preflight;

            let message = if preflight {
                match status {
                    ApplyStatus::Success => {
                        format!("Preflight passed for task {id} (applies cleanly)")
                    }
                    ApplyStatus::Partial => format!(
                        "Preflight: patch does not fully apply for task {id} (applied={}, skipped={}, conflicts={})",
                        r.applied_paths.len(),
                        r.skipped_paths.len(),
                        r.conflicted_paths.len()
                    ),
                    ApplyStatus::Error => format!(
                        "Preflight failed for task {id} (applied={}, skipped={}, conflicts={})",
                        r.applied_paths.len(),
                        r.skipped_paths.len(),
                        r.conflicted_paths.len()
                    ),
                }
            } else {
                match status {
                    ApplyStatus::Success => format!(
                        "Applied task {id} locally ({} files)",
                        r.applied_paths.len()
                    ),
                    ApplyStatus::Partial => format!(
                        "Apply partially succeeded for task {id} (applied={}, skipped={}, conflicts={})",
                        r.applied_paths.len(),
                        r.skipped_paths.len(),
                        r.conflicted_paths.len()
                    ),
                    ApplyStatus::Error => format!(
                        "Apply failed for task {id} (applied={}, skipped={}, conflicts={})",
                        r.applied_paths.len(),
                        r.skipped_paths.len(),
                        r.conflicted_paths.len()
                    ),
                }
            };

            if matches!(status, ApplyStatus::Partial | ApplyStatus::Error)
                || (preflight && !matches!(status, ApplyStatus::Success))
            {
                let mut log = String::new();
                let summary = summarize_patch_for_logging(&diff);
                let mode = if preflight { "preflight" } else { "apply" };
                use std::fmt::Write as _;
                let _ = writeln!(
                    &mut log,
                    "apply_result: mode={} id={} status={:?} applied={} skipped={} conflicts={} cmd={}",
                    mode,
                    id,
                    status,
                    r.applied_paths.len(),
                    r.skipped_paths.len(),
                    r.conflicted_paths.len(),
                    r.cmd_for_log
                );
                let _ = writeln!(
                    &mut log,
                    "stdout_tail=\n{}\nstderr_tail=\n{}",
                    tail(&r.stdout, /*max*/ 2000),
                    tail(&r.stderr, /*max*/ 2000)
                );
                let _ = writeln!(&mut log, "{summary}");
                let _ = writeln!(
                    &mut log,
                    "----- PATCH BEGIN -----\n{diff}\n----- PATCH END -----"
                );
                append_error_log(&log);
            }

            Ok(ApplyOutcome {
                applied,
                status,
                message,
                skipped_paths: r.skipped_paths,
                conflict_paths: r.conflicted_paths,
            })
        }
    }

    fn details_path(base_url: &str, id: &str) -> Option<String> {
        if base_url.contains("/backend-api") {
            Some(format!("{base_url}/wham/tasks/{id}"))
        } else if base_url.contains("/api/codex") {
            Some(format!("{base_url}/tasks/{id}"))
        } else {
            None
        }
    }

    fn extract_assistant_messages_from_body(body: &str) -> Vec<String> {
        let mut msgs = Vec::new();
        if let Ok(full) = serde_json::from_str::<serde_json::Value>(body)
            && let Some(arr) = full
                .get("current_assistant_turn")
                .and_then(|v| v.get("worklog"))
                .and_then(|v| v.get("messages"))
                .and_then(|v| v.as_array())
        {
            for m in arr {
                let is_assistant = m
                    .get("author")
                    .and_then(|a| a.get("role"))
                    .and_then(|r| r.as_str())
                    == Some("assistant");
                if !is_assistant {
                    continue;
                }
                if let Some(parts) = m
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                {
                    for p in parts {
                        if let Some(s) = p.as_str() {
                            if !s.is_empty() {
                                msgs.push(s.to_string());
                            }
                            continue;
                        }
                        if let Some(obj) = p.as_object()
                            && obj.get("content_type").and_then(|t| t.as_str()) == Some("text")
                            && let Some(txt) = obj.get("text").and_then(|t| t.as_str())
                        {
                            msgs.push(txt.to_string());
                        }
                    }
                }
            }
        }
        msgs
    }

    fn turn_attempt_from_map(turn: &HashMap<String, Value>) -> Option<TurnAttempt> {
        let turn_id = turn.get("id").and_then(Value::as_str)?.to_string();
        let attempt_placement = turn.get("attempt_placement").and_then(Value::as_i64);
        let created_at = parse_timestamp_value(turn.get("created_at"));
        let status = attempt_status_from_str(turn.get("turn_status").and_then(Value::as_str));
        let diff = extract_diff_from_turn(turn);
        let messages = extract_assistant_messages_from_turn(turn);
        Some(TurnAttempt {
            turn_id,
            attempt_placement,
            created_at,
            status,
            diff,
            messages,
        })
    }

    fn compare_attempts(a: &TurnAttempt, b: &TurnAttempt) -> Ordering {
        match (a.attempt_placement, b.attempt_placement) {
            (Some(lhs), Some(rhs)) => lhs.cmp(&rhs),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => match (a.created_at, b.created_at) {
                (Some(lhs), Some(rhs)) => lhs.cmp(&rhs),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => a.turn_id.cmp(&b.turn_id),
            },
        }
    }

    fn extract_diff_from_turn(turn: &HashMap<String, Value>) -> Option<String> {
        let items = turn.get("output_items").and_then(Value::as_array)?;
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("output_diff") => {
                    if let Some(diff) = item.get("diff").and_then(Value::as_str)
                        && !diff.is_empty()
                    {
                        return Some(diff.to_string());
                    }
                }
                Some("pr") => {
                    if let Some(diff) = item
                        .get("output_diff")
                        .and_then(Value::as_object)
                        .and_then(|od| od.get("diff"))
                        .and_then(Value::as_str)
                        && !diff.is_empty()
                    {
                        return Some(diff.to_string());
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn extract_assistant_messages_from_turn(turn: &HashMap<String, Value>) -> Vec<String> {
        let mut msgs = Vec::new();
        if let Some(items) = turn.get("output_items").and_then(Value::as_array) {
            for item in items {
                if item.get("type").and_then(Value::as_str) != Some("message") {
                    continue;
                }
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if part.get("content_type").and_then(Value::as_str) == Some("text")
                            && let Some(txt) = part.get("text").and_then(Value::as_str)
                            && !txt.is_empty()
                        {
                            msgs.push(txt.to_string());
                        }
                    }
                }
            }
        }
        msgs
    }

    fn attempt_status_from_str(raw: Option<&str>) -> AttemptStatus {
        match raw.unwrap_or_default() {
            "failed" => AttemptStatus::Failed,
            "completed" => AttemptStatus::Completed,
            "in_progress" => AttemptStatus::InProgress,
            "pending" => AttemptStatus::Pending,
            "cancelled" => AttemptStatus::Cancelled,
            _ => AttemptStatus::Unknown,
        }
    }

    fn parse_timestamp_value(v: Option<&Value>) -> Option<DateTime<Utc>> {
        timestamp_from_seconds(v?.as_f64()?)
    }

    fn timestamp_from_seconds(ts: f64) -> Option<DateTime<Utc>> {
        if !ts.is_finite() {
            return None;
        }
        let seconds = ts.floor();
        let nanos = (((ts - seconds) * 1_000_000_000.0) as u32).min(999_999_999);
        DateTime::from_timestamp(seconds as i64, nanos)
    }

    fn map_task_list_item_to_summary(src: backend::TaskListItem) -> TaskSummary {
        let status_display = src.task_status_display.as_ref();
        TaskSummary {
            id: TaskId(src.id),
            title: src.title,
            status: map_status(status_display),
            updated_at: parse_updated_at(src.updated_at.as_ref()),
            environment_id: None,
            environment_label: env_label_from_status_display(status_display),
            summary: diff_summary_from_status_display(status_display),
            is_review: src
                .pull_requests
                .as_ref()
                .is_some_and(|prs| !prs.is_empty()),
            attempt_total: attempt_total_from_status_display(status_display),
        }
    }

    fn map_status(v: Option<&HashMap<String, Value>>) -> TaskStatus {
        if let Some(val) = v {
            if let Some(turn) = val
                .get("latest_turn_status_display")
                .and_then(Value::as_object)
                && let Some(s) = turn.get("turn_status").and_then(Value::as_str)
            {
                return match s {
                    "failed" => TaskStatus::Error,
                    "completed" => TaskStatus::Ready,
                    "in_progress" => TaskStatus::Pending,
                    "pending" => TaskStatus::Pending,
                    "cancelled" => TaskStatus::Error,
                    _ => TaskStatus::Pending,
                };
            }
            if let Some(state) = val.get("state").and_then(Value::as_str) {
                return match state {
                    "pending" => TaskStatus::Pending,
                    "ready" => TaskStatus::Ready,
                    "applied" => TaskStatus::Applied,
                    "error" => TaskStatus::Error,
                    _ => TaskStatus::Pending,
                };
            }
        }
        TaskStatus::Pending
    }

    fn parse_updated_at(ts: Option<&f64>) -> DateTime<Utc> {
        ts.and_then(|ts| timestamp_from_seconds(*ts))
            .unwrap_or_else(Utc::now)
    }

    fn env_label_from_status_display(v: Option<&HashMap<String, Value>>) -> Option<String> {
        let map = v?;
        map.get("environment_label")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn diff_summary_from_diff(diff: &str) -> DiffSummary {
        let mut files_changed = 0usize;
        let mut lines_added = 0usize;
        let mut lines_removed = 0usize;
        for line in diff.lines() {
            if line.starts_with("diff --git ") {
                files_changed += 1;
                continue;
            }
            if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
                continue;
            }
            match line.as_bytes().first() {
                Some(b'+') => lines_added += 1,
                Some(b'-') => lines_removed += 1,
                _ => {}
            }
        }
        if files_changed == 0 && !diff.trim().is_empty() {
            files_changed = 1;
        }
        DiffSummary {
            files_changed,
            lines_added,
            lines_removed,
        }
    }

    fn diff_summary_from_status_display(v: Option<&HashMap<String, Value>>) -> DiffSummary {
        let mut out = DiffSummary::default();
        let Some(map) = v else { return out };
        let latest = map
            .get("latest_turn_status_display")
            .and_then(Value::as_object);
        let Some(latest) = latest else { return out };
        if let Some(ds) = latest.get("diff_stats").and_then(Value::as_object) {
            if let Some(n) = ds.get("files_modified").and_then(Value::as_i64) {
                out.files_changed = n.max(0) as usize;
            }
            if let Some(n) = ds.get("lines_added").and_then(Value::as_i64) {
                out.lines_added = n.max(0) as usize;
            }
            if let Some(n) = ds.get("lines_removed").and_then(Value::as_i64) {
                out.lines_removed = n.max(0) as usize;
            }
        }
        out
    }

    fn latest_turn_timestamp(v: Option<&HashMap<String, Value>>) -> Option<f64> {
        let map = v?;
        let latest = map
            .get("latest_turn_status_display")
            .and_then(Value::as_object)?;
        latest
            .get("updated_at")
            .or_else(|| latest.get("created_at"))
            .and_then(Value::as_f64)
    }

    fn attempt_total_from_status_display(v: Option<&HashMap<String, Value>>) -> Option<usize> {
        let map = v?;
        let latest = map
            .get("latest_turn_status_display")
            .and_then(Value::as_object)?;
        let siblings = latest.get("sibling_turn_ids").and_then(Value::as_array)?;
        Some(siblings.len().saturating_add(1))
    }

    fn is_unified_diff(diff: &str) -> bool {
        let t = diff.trim_start();
        if t.starts_with("diff --git ") {
            return true;
        }
        let has_dash_headers = diff.contains("\n--- ") && diff.contains("\n+++ ");
        let has_hunk = diff.contains("\n@@ ") || diff.starts_with("@@ ");
        has_dash_headers && has_hunk
    }

    fn tail(s: &str, max: usize) -> String {
        if s.len() <= max {
            s.to_string()
        } else {
            s[s.ceil_char_boundary(s.len() - max)..].to_string()
        }
    }

    fn summarize_patch_for_logging(patch: &str) -> String {
        let trimmed = patch.trim_start();
        let kind = if trimmed.starts_with("*** Begin Patch") {
            "codex-patch"
        } else if trimmed.starts_with("diff --git ") || trimmed.contains("\n*** End Patch\n") {
            "git-diff"
        } else if trimmed.starts_with("@@ ") || trimmed.contains("\n@@ ") {
            "unified-diff"
        } else {
            "unknown"
        };
        let lines = patch.lines().count();
        let chars = patch.len();
        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        let head: String = patch.lines().take(20).collect::<Vec<&str>>().join("\n");
        let head_trunc = if head.len() > 800 {
            format!("{}…", &head[..head.floor_char_boundary(800)])
        } else {
            head
        };
        format!(
            "patch_summary: kind={kind} lines={lines} chars={chars} cwd={cwd} ; head=\n{head_trunc}"
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        async fn client_with_json_responses(
            responses: Vec<(&'static str, Value)>,
        ) -> (HttpClient, tokio::task::JoinHandle<()>) {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("HTTP listener");
            let base_url = format!("http://{}", listener.local_addr().expect("server address"));
            let server = tokio::spawn(async move {
                for (expected_path, response) in responses {
                    let (mut stream, _) = listener.accept().await.expect("HTTP request");
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let byte = stream.read_u8().await.expect("HTTP request header");
                        request.push(byte);
                        assert!(request.len() < 16_384, "bounded request header");
                    }
                    assert!(
                        String::from_utf8(request)
                            .expect("HTTP header")
                            .starts_with(&format!("GET {expected_path} HTTP/1.1\r\n"))
                    );
                    let body = response.to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream
                        .write_all(response.as_bytes())
                        .await
                        .expect("HTTP response");
                }
            });
            let client = HttpClient::new(
                base_url,
                codex_http_client::HttpClientFactory::new(
                    codex_http_client::OutboundProxyPolicy::ReqwestDefault,
                ),
            );
            (client, server)
        }

        #[tokio::test]
        async fn cloud_backend_decodes_cancelled_and_unknown_attempt_statuses() {
            let turns = [
                ("failed", AttemptStatus::Failed),
                ("completed", AttemptStatus::Completed),
                ("in_progress", AttemptStatus::InProgress),
                ("pending", AttemptStatus::Pending),
                ("cancelled", AttemptStatus::Cancelled),
                ("future_status", AttemptStatus::Unknown),
            ];
            let body = serde_json::json!({
                "sibling_turns": turns.iter().enumerate().map(|(index, (status, _))| {
                    serde_json::json!({"id": status, "attempt_placement": index, "turn_status": status})
                }).collect::<Vec<_>>()
            });
            let (client, server) = client_with_json_responses(vec![(
                "/api/codex/tasks/task/turns/turn/sibling_turns",
                body,
            )])
            .await;
            let attempts = client
                .list_sibling_attempts(TaskId("task".to_string()), "turn".to_string())
                .await
                .expect("public sibling-turn response");
            server.await.expect("HTTP server");
            assert_eq!(attempts.len(), turns.len());
            for (attempt, (id, status)) in attempts.iter().zip(turns) {
                assert_eq!(attempt.turn_id, id);
                assert_eq!(attempt.status, status);
            }
        }

        #[tokio::test]
        async fn cloud_backend_decodes_timestamps_without_panicking_or_losing_pre_epoch_time() {
            let (client, server) = client_with_json_responses(vec![
                (
                    "/api/codex/tasks/task/turns/turn/sibling_turns",
                    serde_json::json!({
                        "sibling_turns": [
                            {"id": "negative", "attempt_placement": 0, "created_at": -1.25},
                            {"id": "positive", "attempt_placement": 1, "created_at": 1.25},
                            {"id": "system_overflow", "attempt_placement": 2, "created_at": 1e300},
                            {"id": "chrono_overflow", "attempt_placement": 3, "created_at": 1e13},
                            {"id": "missing", "attempt_placement": 4}
                        ]
                    }),
                ),
                (
                    "/api/codex/tasks/task",
                    serde_json::json!({
                        "task": {"id": "task", "title": "Invalid timestamp", "archived": false,
                            "external_pull_requests": [], "created_at": 1e300}
                    }),
                ),
            ])
            .await;
            let attempts = client
                .list_sibling_attempts(TaskId("task".to_string()), "turn".to_string())
                .await
                .expect("public sibling-turn response");
            assert_eq!(attempts.len(), 5);
            assert_eq!(
                attempts[0].created_at,
                DateTime::from_timestamp(-2, 750_000_000)
            );
            assert_eq!(
                attempts[1].created_at,
                DateTime::from_timestamp(1, 250_000_000)
            );
            for attempt in &attempts[2..] {
                assert_eq!(
                    attempt.created_at, None,
                    "{} has no usable timestamp",
                    attempt.turn_id
                );
            }
            let before = Utc::now();
            let summary = client
                .get_task_summary(TaskId("task".to_string()))
                .await
                .expect("public task summary");
            let after = Utc::now();
            server.await.expect("HTTP server");
            assert_eq!(summary.id, TaskId("task".to_string()));
            assert_eq!(summary.title, "Invalid timestamp");
            assert!(before <= summary.updated_at && summary.updated_at <= after);
        }

        #[test]
        fn tail_respects_byte_budget_without_splitting_unicode() {
            for (input, budget, expected) in [
                ("abc", 2, "bc"),
                ("a雪b", 3, "b"),
                ("a雪b", 4, "雪b"),
                ("a雪b", 5, "a雪b"),
                ("雪", 2, ""),
                ("雪", 0, ""),
                ("", 2, ""),
            ] {
                assert_eq!(tail(input, budget), expected);
            }
        }

        #[test]
        fn patch_summary_truncates_at_a_unicode_boundary() {
            let patch = format!("{}雪suffix", "a".repeat(799));
            let summary = summarize_patch_for_logging(&patch);
            assert_eq!(
                summary.split_once(" ; head=\n").unwrap().1,
                format!("{}…", "a".repeat(799))
            );
            let short_patch = "雪\nunchanged";
            assert_eq!(
                summarize_patch_for_logging(short_patch)
                    .split_once(" ; head=\n")
                    .unwrap()
                    .1,
                short_patch
            );
        }

        #[tokio::test]
        async fn apply_task_rejects_long_unicode_non_unified_diff() {
            let client = HttpClient::new(
                "http://127.0.0.1:1",
                codex_http_client::HttpClientFactory::new(
                    codex_http_client::OutboundProxyPolicy::ReqwestDefault,
                ),
            );
            let patch = "雪".repeat(300);
            for preflight in [false, true] {
                let task = TaskId("unicode-invalid-diff".to_string());
                let outcome = if preflight {
                    client.apply_task_preflight(task, Some(patch.clone())).await
                } else {
                    client.apply_task(task, Some(patch.clone())).await
                }
                .expect("invalid diffs return an application outcome");
                assert_eq!(
                    outcome,
                    ApplyOutcome {
                        applied: false,
                        status: ApplyStatus::Error,
                        message:
                            "Expected unified git diff; backend returned an incompatible format."
                                .to_string(),
                        skipped_paths: Vec::new(),
                        conflict_paths: Vec::new(),
                    }
                );
            }
        }

        #[tokio::test(flavor = "current_thread")]
        async fn apply_task_and_preflight_keep_executor_responsive_and_preserve_index() {
            use std::path::Path;
            use std::process::Command;
            use std::sync::atomic::AtomicBool;
            use std::sync::atomic::Ordering;
            use std::time::Duration;

            struct RestoreDirectory(std::path::PathBuf);
            impl Drop for RestoreDirectory {
                fn drop(&mut self) {
                    std::env::set_current_dir(&self.0).expect("restore current directory");
                }
            }

            fn git(cwd: &Path, args: &[&str]) -> String {
                let output = Command::new("git")
                    .args(args)
                    .current_dir(cwd)
                    .output()
                    .expect("run git");
                assert!(
                    output.status.success(),
                    "git {args:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                String::from_utf8(output.stdout).expect("Git output is UTF-8")
            }

            let directory = tempfile::tempdir().expect("temporary repository");
            let cwd = directory.path();
            git(cwd, &["init", "--quiet"]);
            git(cwd, &["config", "user.email", "test@example.com"]);
            git(cwd, &["config", "user.name", "Cloud task test"]);
            git(cwd, &["config", "core.autocrlf", "false"]);
            std::fs::write(cwd.join("file.txt"), "before\n").expect("original file");
            git(cwd, &["add", "file.txt"]);
            git(cwd, &["commit", "--quiet", "-m", "initial"]);
            std::fs::write(cwd.join("file.txt"), "after\n").expect("changed file");
            let patch = git(cwd, &["diff", "--binary"]);
            std::fs::write(cwd.join("file.txt"), "before\n").expect("restore file");
            // Git's normal clean-filter path supplies a real slow subprocess on
            // both Windows (Git's sh) and Unix, without replacing patch logic.
            std::fs::write(cwd.join(".gitattributes"), "file.txt filter=slow\n")
                .expect("filter attributes");
            git(cwd, &["config", "filter.slow.clean", "sleep 0.5; cat"]);
            git(cwd, &["config", "filter.slow.smudge", "cat"]);
            git(cwd, &["config", "filter.slow.required", "true"]);
            let original_index = std::fs::read(cwd.join(".git/index")).expect("original index");
            let _restore = RestoreDirectory(std::env::current_dir().expect("current directory"));
            std::env::set_current_dir(cwd).expect("enter test repository");
            let client = HttpClient::new(
                "http://127.0.0.1:1",
                codex_http_client::HttpClientFactory::new(
                    codex_http_client::OutboundProxyPolicy::ReqwestDefault,
                ),
            );

            for preflight in [true, false] {
                let completed = AtomicBool::new(false);
                let apply = async {
                    let task = TaskId("responsive-apply".to_string());
                    let result = if preflight {
                        client.apply_task_preflight(task, Some(patch.clone())).await
                    } else {
                        client.apply_task(task, Some(patch.clone())).await
                    };
                    completed.store(true, Ordering::SeqCst);
                    result
                };
                let observe_executor = async {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    !completed.load(Ordering::SeqCst)
                };
                let (result, progressed_while_git_running) =
                    tokio::join!(biased; apply, observe_executor);
                let outcome = result.expect("apply outcome");
                assert!(
                    progressed_while_git_running,
                    "Git blocked the async executor"
                );
                assert_eq!(
                    outcome.status,
                    ApplyStatus::Success,
                    "preflight={preflight}, outcome={outcome:?}, log={:?}",
                    std::fs::read_to_string(cwd.join("error.log"))
                );
                assert_eq!(outcome.applied, !preflight);
                assert!(outcome.skipped_paths.is_empty());
                assert!(outcome.conflict_paths.is_empty());
                assert_eq!(
                    std::fs::read_to_string(cwd.join("file.txt")).expect("resulting file"),
                    if preflight { "before\n" } else { "after\n" }
                );
                assert_eq!(
                    std::fs::read(cwd.join(".git/index")).expect("resulting index"),
                    original_index
                );
            }

            std::fs::write(cwd.join("file.txt"), "before\n").expect("reset for cancellation");
            git(
                cwd,
                &[
                    "config",
                    "filter.slow.clean",
                    "printf '%s' \"$GIT_INDEX_FILE\" > .git/clean-index-path; sleep 0.5; cat",
                ],
            );
            let mut cancelled_apply =
                client.apply_task(TaskId("cancelled-apply".to_string()), Some(patch));
            let private_index = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    tokio::select! {
                        result = &mut cancelled_apply => {
                            panic!("apply completed before filter signalled: {result:?}");
                        }
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {
                            if let Ok(path) = std::fs::read_to_string(cwd.join(".git/clean-index-path"))
                                && !path.is_empty()
                            {
                                break std::path::PathBuf::from(path);
                            }
                        }
                    }
                }
            })
            .await
            .expect("real Git filter must start");
            assert_ne!(private_index, cwd.join(".git/index"));
            assert!(private_index.is_file());
            drop(cancelled_apply);

            let private_index_directory = private_index.parent().expect("private index directory");
            tokio::time::timeout(Duration::from_secs(10), async {
                while private_index_directory.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect(
                "worker must finish and remove its private Git index after caller cancellation",
            );
            assert_eq!(
                std::fs::read_to_string(cwd.join("file.txt")).expect("completed cancelled apply"),
                "after\n"
            );
            assert_eq!(
                std::fs::read(cwd.join(".git/index")).expect("real index after cancellation"),
                original_index
            );
        }
    }
}
