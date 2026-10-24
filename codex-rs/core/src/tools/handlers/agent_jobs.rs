use crate::FunctionCallError;
use crate::agent::control::AgentJobBinding;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::role::AgentRoleLocks;
use crate::agent::status::is_final;
use crate::config::Config;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::multi_agents_common::apply_spawn_agent_model_defaults_and_overrides;
use crate::tools::handlers::multi_agents_common::apply_spawn_agent_service_tier;
use crate::tools::handlers::multi_agents_common::build_agent_spawn_config_for_step;
use crate::tools::handlers::multi_agents_common::inherit_live_service_tier;
use crate::tools::handlers::parse_arguments;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use tokio::sync::watch::Receiver;
use tokio::time::Duration;
use tokio::time::Instant;
#[cfg(test)]
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use uuid::Uuid;

mod report_agent_job_result;
mod spawn_agents_on_csv;

pub use report_agent_job_result::ReportAgentJobResultHandler;
pub use spawn_agents_on_csv::SpawnAgentsOnCsvHandler;

const DEFAULT_AGENT_JOB_CONCURRENCY: usize = 16;
const MAX_AGENT_JOB_CONCURRENCY: usize = 64;
const MISSING_STATUS_RECHECK_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_AGENT_JOB_ITEM_TIMEOUT: Duration = Duration::from_secs(60 * 30);
const ORPHANED_AGENT_JOB_RECONCILIATION_INTERVAL: Duration = Duration::from_secs(5);
const PARENT_TOOL_CANCELLATION_REASON: &str = "cancelled by parent tool request";
const MAX_SCHEMA_VALIDATION_ERRORS: usize = 5;
const AGENT_JOB_OUTPUT_COLUMNS: [&str; 10] = [
    "job_id",
    "item_id",
    "row_index",
    "source_id",
    "status",
    "attempt_count",
    "last_error",
    "result_json",
    "reported_at",
    "completed_at",
];
static ACTIVE_AGENT_JOB_RECONCILERS: LazyLock<std::sync::Mutex<HashSet<String>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

#[derive(Debug, Deserialize)]
struct SpawnAgentsOnCsvArgs {
    csv_path: String,
    instruction: String,
    id_column: Option<String>,
    output_csv_path: Option<String>,
    output_schema: Option<Value>,
    max_concurrency: Option<usize>,
    max_workers: Option<usize>,
    max_runtime_seconds: Option<u64>,
    #[serde(default)]
    deliver: bool,
}

#[derive(Debug, Deserialize)]
struct ReportAgentJobResultArgs {
    job_id: String,
    item_id: String,
    result: Value,
    stop: Option<bool>,
    #[serde(default)]
    deliver: bool,
}

#[derive(Debug, Serialize)]
struct SpawnAgentsOnCsvResult {
    job_id: String,
    status: String,
    output_csv_path: String,
    output_sha256: Option<String>,
    total_items: usize,
    completed_items: usize,
    failed_items: usize,
    job_error: Option<String>,
    failed_item_errors: Option<Vec<AgentJobFailureSummary>>,
}

#[derive(Debug, Serialize)]
struct AgentJobFailureSummary {
    item_id: String,
    source_id: Option<String>,
    last_error: String,
}

#[derive(Debug, Serialize)]
struct ReportAgentJobResultToolResult {
    accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    acknowledgment: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rejection_reason: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct JobRunnerOptions {
    max_concurrency: usize,
    spawn_config: Config,
    environments: TurnEnvironmentSnapshot,
}

#[derive(Debug, Clone)]
struct ActiveJobItem {
    item_id: String,
    started_at: Instant,
    status_rx: Option<Receiver<AgentStatus>>,
}

struct AgentJobCleanupOwner {
    session: Arc<Session>,
    db: Arc<codex_state::StateRuntime>,
    job_id: String,
    active_items: HashMap<ThreadId, ActiveJobItem>,
    armed: bool,
}

impl Drop for AgentJobCleanupOwner {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let session = self.session.clone();
        let db = self.db.clone();
        let job_id = self.job_id.clone();
        let mut active_items = std::mem::take(&mut self.active_items);
        self.session.terminal_tasks.spawn(async move {
            loop {
                // Conditional terminalization preserves cancellation, accepted
                // results, and a foreign reaper's original failure reason.
                let persistence = match db.agent_job_is_owned(&job_id).await {
                    Ok(true) => {
                        db.mark_agent_job_failed(&job_id, "agent job owner ended before completion")
                            .await
                    }
                    Ok(false) => Ok(()),
                    Err(error) => Err(error),
                };
                let cleanup = terminate_agent_job_workers(
                    session.clone(),
                    db.clone(),
                    &job_id,
                    &mut active_items,
                    "job owner ended before worker completion",
                )
                .await;
                if persistence.is_ok() && cleanup.is_ok() {
                    if let Ok(Some(job)) = db.get_agent_job(&job_id).await
                        && job.status.is_final()
                        && let Err(error) = export_job_csv_snapshot(db.clone(), &job).await
                    {
                        tracing::warn!(%job_id, %error, "late job export remains pending");
                    }
                    break;
                }
                // Individual and tree shutdown already have durable owners.
                // Wait for those, not repeated timed-out shutdown invocations.
                let mut waits = futures::stream::iter(active_items.keys().copied())
                    .map(|thread| {
                        session
                            .services
                            .agent_control
                            .wait_for_agent_tree_termination(thread)
                    })
                    .buffer_unordered(8);
                let mut drained = true;
                while let Some(result) = waits.next().await {
                    if let Err(error) = result {
                        drained = false;
                        tracing::warn!(%job_id, %error, "waiting for late worker cleanup");
                    }
                }
                // A closed store cannot acknowledge settlement. Leave its
                // durable bindings for restart only AFTER actual worker drain.
                if db.is_closed() && drained {
                    break;
                }
                tokio::time::sleep(ORPHANED_AGENT_JOB_RECONCILIATION_INTERVAL).await;
            }
        });
    }
}

fn required_state_db(
    session: &Arc<Session>,
) -> Result<Arc<codex_state::StateRuntime>, FunctionCallError> {
    session.state_db().ok_or_else(|| {
        FunctionCallError::RespondToModel("csv_backend_unavailable: CSV jobs require persistent session state; ephemeral sessions are unsupported and a failed persistent backend must be restored before retrying".to_string())
    })
}

async fn build_runner_options(
    session: &Arc<Session>,
    step: &StepContext,
    requested_concurrency: Option<usize>,
) -> Result<JobRunnerOptions, FunctionCallError> {
    let turn = &step.turn;
    let multi_agent_version = turn.multi_agent_version;
    if multi_agent_version == MultiAgentVersion::Disabled {
        return Err(FunctionCallError::RespondToModel(
            "multi-agent runtime is disabled; this session cannot spawn workers".to_string(),
        ));
    }
    let agent_max_threads = turn.config.effective_agent_max_threads(multi_agent_version);
    if agent_max_threads == Some(0) {
        return Err(FunctionCallError::RespondToModel(
            "agent thread limit reached; this session cannot spawn more subagents".to_string(),
        ));
    }
    let max_concurrency = normalize_concurrency(requested_concurrency, agent_max_threads);
    let mut spawn_config = build_agent_spawn_config_for_step(step)?;
    spawn_config.cwd = spawn_agents_on_csv::single_local_environment_cwd(&step.environments)?;
    let parent_service_tier = inherit_live_service_tier(session, &mut spawn_config).await;
    apply_spawn_agent_model_defaults_and_overrides(
        session,
        turn.as_ref(),
        &mut spawn_config,
        /*requested_model*/ None,
        /*requested_reasoning_effort*/ None,
        AgentRoleLocks::default(),
    )
    .await?;
    apply_spawn_agent_service_tier(
        session,
        turn.as_ref(),
        &mut spawn_config,
        parent_service_tier.as_deref(),
        /*requested_service_tier*/ None,
    )
    .await?;
    Ok(JobRunnerOptions {
        max_concurrency,
        spawn_config,
        environments: step.environments.clone(),
    })
}

fn normalize_concurrency(requested: Option<usize>, max_threads: Option<usize>) -> usize {
    let requested = requested.unwrap_or(DEFAULT_AGENT_JOB_CONCURRENCY).max(1);
    let requested = requested.min(MAX_AGENT_JOB_CONCURRENCY);
    if let Some(max_threads) = max_threads {
        requested.min(max_threads.max(1))
    } else {
        requested
    }
}

fn normalize_max_runtime_seconds(requested: Option<u64>) -> Result<Option<u64>, FunctionCallError> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    if requested == 0 {
        return Err(FunctionCallError::RespondToModel(
            "max_runtime_seconds must be >= 1".to_string(),
        ));
    }
    Ok(Some(requested))
}

fn validate_agent_job_output_schema(schema: &Value) -> Result<(), FunctionCallError> {
    if !schema.is_object() {
        return Err(FunctionCallError::RespondToModel(
            "output_schema must be a JSON Schema object".to_string(),
        ));
    }
    jsonschema::meta::validate(schema).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "output_schema is not a valid JSON Schema: {error}"
        ))
    })?;
    let root_allows_object = match schema.get("type") {
        None => true,
        Some(Value::String(schema_type)) => schema_type == "object",
        Some(Value::Array(schema_types)) => schema_types
            .iter()
            .any(|schema_type| schema_type.as_str() == Some("object")),
        Some(_) => false,
    };
    if !root_allows_object {
        return Err(FunctionCallError::RespondToModel(
            "output_schema root type must allow JSON object results".to_string(),
        ));
    }
    jsonschema::validator_for(schema).map_err(|error| {
        FunctionCallError::RespondToModel(format!("output_schema could not be compiled: {error}"))
    })?;
    Ok(())
}

fn validate_agent_job_result(schema: &Value, result: &Value) -> Result<(), FunctionCallError> {
    let validator = jsonschema::validator_for(schema).map_err(|error| {
        FunctionCallError::Fatal(format!(
            "stored agent job output_schema could not be compiled: {error}"
        ))
    })?;
    let mut errors = validator.iter_errors(result);
    let mut messages = Vec::new();
    for error in errors.by_ref().take(MAX_SCHEMA_VALIDATION_ERRORS) {
        let pointer = error.instance_path().as_str();
        let path = if pointer.is_empty() {
            "$".to_string()
        } else {
            format!("${pointer}")
        };
        messages.push(format!("{path}: {error}"));
    }
    if messages.is_empty() {
        return Ok(());
    }
    if errors.next().is_some() {
        messages.push("additional validation errors omitted".to_string());
    }
    Err(FunctionCallError::RespondToModel(format!(
        "result does not match output_schema: {}",
        messages.join("; ")
    )))
}

async fn observe_parent_job_cancellation(
    cancellation_token: &CancellationToken,
    db: &codex_state::StateRuntime,
    job_id: &str,
) -> anyhow::Result<bool> {
    if !cancellation_token.is_cancelled() {
        return Ok(false);
    }
    db.mark_agent_job_cancelled(job_id, PARENT_TOOL_CANCELLATION_REASON)
        .await?;
    Ok(true)
}

async fn run_agent_job_loop(
    session: Arc<Session>,
    _turn: Arc<TurnContext>,
    db: Arc<codex_state::StateRuntime>,
    job_id: String,
    options: JobRunnerOptions,
    cancellation_token: CancellationToken,
    active_items: &mut HashMap<ThreadId, ActiveJobItem>,
) -> anyhow::Result<()> {
    let job = db
        .get_agent_job(job_id.as_str())
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent job {job_id} was not found"))?;
    let _cancellation_observer = AbortOnDropHandle::new(tokio::spawn({
        let cancellation_token = cancellation_token.clone();
        let db = db.clone();
        let job_id = job_id.clone();
        async move {
            cancellation_token.cancelled().await;
            if let Err(error) = db
                .mark_agent_job_cancelled(job_id.as_str(), PARENT_TOOL_CANCELLATION_REASON)
                .await
            {
                tracing::warn!(
                    %job_id,
                    %error,
                    "failed to persist parent cancellation for agent job"
                );
            }
        }
    }));
    let runtime_timeout = job_runtime_timeout(&job);
    if db.agent_job_is_owned(job_id.as_str()).await? {
        recover_running_items(
            session.clone(),
            db.clone(),
            job_id.as_str(),
            active_items,
            runtime_timeout,
        )
        .await?;
    }

    let mut job_changes = db.subscribe_agent_job_changes();
    let mut capacity_changes = session.services.agent_control.subscribe_capacity_changes();
    let mut cancel_requested = !db.agent_job_is_owned_running(job_id.as_str()).await?;
    loop {
        // Consume notifications before inspecting state so a concurrent commit
        // between inspection and waiting cannot be lost.
        job_changes.borrow_and_update();
        capacity_changes.borrow_and_update();
        let mut progressed = false;

        if observe_parent_job_cancellation(&cancellation_token, db.as_ref(), job_id.as_str())
            .await?
        {
            cancel_requested = true;
        }

        if !cancel_requested && !db.agent_job_is_owned_running(job_id.as_str()).await? {
            cancel_requested = true;
        }

        if cancel_requested {
            break;
        }

        if active_items.len() < options.max_concurrency {
            // Reserve distinct pending rows for this batch before any spawn is
            // polled. Initializers count against the same worker capacity.
            let slots = (options.max_concurrency - active_items.len()).min(8);
            let pending_items = db
                .list_agent_job_items(
                    &job_id,
                    Some(codex_state::AgentJobItemStatus::Pending),
                    Some(slots),
                )
                .await?;
            let mut spawns = FuturesUnordered::new();
            for item in pending_items {
                let prompt = build_worker_prompt(&job, &item)?;
                let session = session.clone();
                let db = db.clone();
                let job_id = job_id.clone();
                let spawn_config = options.spawn_config.clone();
                let environments = options.environments.clone();
                let cancellation = cancellation_token.clone();
                spawns.push(async move {
                    if cancellation.is_cancelled() {
                        return (item.item_id, Err(CodexErr::TurnAborted));
                    }
                    match db.agent_job_is_owned_running(&job_id).await {
                        Ok(true) => {}
                        Ok(false) => return (item.item_id, Err(CodexErr::TurnAborted)),
                        Err(error) => {
                            return (
                                item.item_id,
                                Err(CodexErr::Fatal(format!(
                                    "failed to check agent job ownership before spawn: {error}"
                                ))),
                            );
                        }
                    }
                    if cancellation.is_cancelled() {
                        return (item.item_id, Err(CodexErr::TurnAborted));
                    }
                    let result = session
                        .services
                        .agent_control
                        .spawn_agent_with_metadata(
                            spawn_config,
                            vec![UserInput::Text {
                                text: prompt,
                                text_elements: Vec::new(),
                            }],
                            Some(SessionSource::SubAgent(SubAgentSource::Other(format!(
                                "agent_job:{job_id}"
                            )))),
                            SpawnAgentOptions {
                                parent_thread_id: Some(session.thread_id),
                                environments: Some(environments.to_selections()),
                                captured_environments: Some(environments),
                                agent_job_binding: Some(AgentJobBinding {
                                    state_db: db,
                                    job_id,
                                    item_id: item.item_id.clone(),
                                    runtime_timeout: Some(runtime_timeout),
                                }),
                                ..Default::default()
                            },
                        )
                        .await;
                    (item.item_id, result)
                });
            }
            let mut failed_spawns = std::collections::VecDeque::new();
            while !spawns.is_empty() || !failed_spawns.is_empty() {
                let completion = if spawns.is_empty() {
                    failed_spawns.pop_front()
                } else {
                    tokio::select! {
                        completion = spawns.next() => completion,
                        _ = wait_for_status_change(
                            active_items, &mut job_changes, &mut capacity_changes,
                            &cancellation_token, runtime_timeout,
                        ) => {
                            let mut completed = Vec::new();
                            let runnable = {
                                let service = service_active_items_during_spawn(
                                    session.clone(), db.clone(), &job_id,
                                    &cancellation_token, active_items, runtime_timeout,
                                );
                                tokio::pin!(service);
                                loop {
                                    // Continue polling every initializer while servicing
                                    // workers: either side can need a DB lock held by the
                                    // other. Retain every completed spawn until handoff.
                                    tokio::select! {
                                        Some(result) = spawns.next(), if !spawns.is_empty() => {
                                            completed.push(result);
                                        }
                                        result = &mut service => break result,
                                    }
                                }
                            };
                            let mut subscribe = Vec::new();
                            for (item_id, result) in completed {
                                match result {
                                    Ok(agent) => {
                                        active_items.insert(agent.thread_id, ActiveJobItem {
                                            item_id, started_at: agent.execution_started_at, status_rx: None,
                                        });
                                        subscribe.push(agent.thread_id);
                                        progressed = true;
                                    }
                                    Err(error) => failed_spawns.push_back((item_id, Err(error))),
                                }
                            }
                            // Register successes before either the error or stop path.
                            // Dropping unfinished spawn futures retains their existing
                            // PendingSpawnCleanup rollback owners, including after bind.
                            if !runnable? {
                                cancel_requested = true;
                                break;
                            }
                            for thread_id in subscribe {
                                if let Some(item) = active_items.get_mut(&thread_id) {
                                    item.status_rx = session.services.agent_control
                                        .subscribe_status(thread_id).await.ok();
                                }
                            }
                            continue;
                        }
                    }
                };
                let Some((item_id, spawn_result)) = completion else {
                    break;
                };
                // A sibling initializer may own a database transaction until
                // its future is polled again. Do not suspend that owner while
                // settling a failed row; drain the bounded batch first. The
                // service branch above continues checking cancellation and
                // worker deadlines while polling every remaining initializer.
                if spawn_result.is_err() && !spawns.is_empty() {
                    failed_spawns.push_back((item_id, spawn_result));
                    continue;
                }
                match spawn_result {
                    Ok(agent) => {
                        let thread_id = agent.thread_id;
                        // No await between spawn completion and cleanup ownership.
                        active_items.insert(
                            thread_id,
                            ActiveJobItem {
                                item_id,
                                started_at: agent.execution_started_at,
                                status_rx: None,
                            },
                        );
                        progressed = true;
                        let status_rx = session
                            .services
                            .agent_control
                            .subscribe_status(thread_id)
                            .await
                            .ok();
                        if let Some(item) = active_items.get_mut(&thread_id) {
                            item.status_rx = status_rx;
                        }
                    }
                    Err(CodexErr::AgentLimitReached { .. }) => {
                        // Other admitted initializers still need to be consumed.
                        // Leave this row pending until the capacity notification.
                    }
                    Err(error) => {
                        if observe_parent_job_cancellation(
                            &cancellation_token,
                            db.as_ref(),
                            &job_id,
                        )
                        .await?
                            || !db.agent_job_is_owned_running(&job_id).await?
                        {
                            cancel_requested = true;
                            break;
                        }
                        let message = format!("failed to spawn worker: {error}");
                        if !db
                            .mark_agent_job_item_spawn_failed(&job_id, &item_id, &message)
                            .await?
                        {
                            if !db.agent_job_is_owned_running(&job_id).await? {
                                cancel_requested = true;
                                break;
                            }
                            anyhow::bail!(
                                "{message}; item was no longer pending or bound to the worker"
                            );
                        }
                        progressed = true;
                    }
                }
                // While initializers remain, only the concurrent service
                // branch may await database work. A completed sibling must
                // not stop polling an initializer that holds its write lock.
                if !spawns.is_empty() {
                    continue;
                }
                if observe_parent_job_cancellation(&cancellation_token, db.as_ref(), &job_id)
                    .await?
                    || !db.agent_job_is_owned_running(&job_id).await?
                {
                    cancel_requested = true;
                    break;
                }
            }
        }

        if cancel_requested {
            break;
        }

        if reap_stale_active_items(
            session.clone(),
            db.clone(),
            job_id.as_str(),
            active_items,
            runtime_timeout,
        )
        .await?
        {
            progressed = true;
        }

        let finished = find_finished_threads(session.clone(), active_items).await;
        if finished.is_empty() {
            if active_items.is_empty()
                && !db.agent_job_has_unfinished_items(job_id.as_str()).await?
            {
                break;
            }
            if !progressed {
                wait_for_status_change(
                    active_items,
                    &mut job_changes,
                    &mut capacity_changes,
                    &cancellation_token,
                    runtime_timeout,
                )
                .await;
            }
            continue;
        }

        settle_active_job_workers(
            session.clone(),
            db.clone(),
            &job_id,
            active_items,
            finished,
            "worker finished without calling report_agent_job_result",
        )
        .await?;
    }

    if observe_parent_job_cancellation(&cancellation_token, db.as_ref(), job_id.as_str()).await? {
        cancel_requested = true;
    }
    if !cancel_requested && !db.agent_job_is_owned_running(job_id.as_str()).await? {
        cancel_requested = true;
    }

    let cleanup_error = if cancel_requested {
        terminate_agent_job_workers(
            session.clone(),
            db.clone(),
            job_id.as_str(),
            active_items,
            "job cancelled before worker completion",
        )
        .await
        .err()
    } else {
        None
    };
    let export_error = if let Some(current) = db.get_agent_job(&job_id).await?
        && (current.status.is_final() || db.agent_job_is_owned(&job_id).await?)
    {
        export_job_csv_snapshot(db.clone(), &current).await.err()
    } else {
        None
    };
    if let Some(cleanup_error) = cleanup_error {
        if let Some(export_error) = export_error {
            return Err(anyhow::anyhow!(
                "failed to terminate workers for agent job {job_id}: {cleanup_error}; final export also failed: {export_error}"
            ));
        }
        return Err(anyhow::anyhow!(
            "failed to terminate workers for agent job {job_id}: {cleanup_error}"
        ));
    }
    if let Some(export_error) = export_error {
        if cancel_requested {
            return Err(anyhow::anyhow!(
                "failed to export cancelled agent job {job_id}: {export_error}"
            ));
        }
        let message = format!("auto-export failed: {export_error}");
        db.mark_agent_job_failed(job_id.as_str(), message.as_str())
            .await?;
        return Ok(());
    }
    if cancel_requested {
        return Ok(());
    }
    db.mark_agent_job_completed(job_id.as_str()).await?;
    Ok(())
}

async fn export_job_csv_snapshot(
    db: Arc<codex_state::StateRuntime>,
    job: &codex_state::AgentJob,
) -> anyhow::Result<()> {
    let job = job.clone();
    // This owner retains the file lock through acknowledgment even when its
    // observer is cancelled. Never lease a slow writer's lock to another task.
    tokio::spawn(async move {
        let output_path = PathBuf::from(&job.output_csv_path);
        let (write_path, lock) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let paths = codex_file_system::resolve_symlink_write_paths(&output_path)?;
            // Normalize parent aliases (and existing filename casing) before
            // both locking and durable ownership. Do not fold case: Windows
            // directories can be case-sensitive.
            let parent = paths.write_path.parent().ok_or_else(|| anyhow::anyhow!("CSV destination has no parent"))?;
            std::fs::create_dir_all(parent)?;
            let filename = paths.write_path.file_name().ok_or_else(|| anyhow::anyhow!("CSV destination has no filename"))?;
            let write_path = std::fs::canonicalize(parent)?.join(filename);
            let write_path = match std::fs::canonicalize(&write_path) {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => write_path,
                Err(error) => return Err(error.into()),
            };
            let lock = codex_file_system::try_acquire_atomic_write_lock(&write_path)?
                .ok_or_else(|| {
                    anyhow::anyhow!("CSV publication is busy; export remains pending")
                })?;
            Ok((write_path, lock))
        })
        .await??;
        let destination = write_path.to_string_lossy().into_owned();
        db.reserve_agent_job_destination(&job.id, &destination).await?;
        // Snapshot selection occurs AFTER destination ownership, not before.
        if db.agent_job_export_is_published(&job.id).await?
            && tokio::fs::try_exists(&write_path).await?
        {
            return Ok(());
        }
        let (revision, items) = db.agent_job_export_snapshot(&job.id).await?;
        let csv = render_job_csv(&job.input_headers, &items)
            .map_err(|err| anyhow::anyhow!("failed to render job csv for auto-export: {err}"))?;
        let digest = <sha2::Sha256 as sha2::Digest>::digest(csv.as_bytes());
        let digest = hex::encode(digest);
        write_job_csv_atomically(write_path, csv).await?;
        db.acknowledge_agent_job_artifact(&job.id, revision, &destination, &digest).await?;
        drop(lock);
        Ok(())
    })
    .await?
}

pub(crate) async fn reconcile_orphaned_agent_jobs_after_restart(
    db: Arc<codex_state::StateRuntime>,
) -> anyhow::Result<()> {
    // The terminal transaction retains export obligations even if loading its
    // returned job summaries fails. Drain previously pending exports regardless.
    let reconciliation = db.reconcile_orphaned_agent_jobs_after_restart().await;
    let pending = db.claim_agent_job_recovery_exports().await?;
    let mut export_errors = Vec::new();
    if let Err(error) = reconciliation {
        export_errors.push(error.to_string());
    }
    let mut exports = futures::stream::iter(pending)
        .map(|(job_id, token)| {
            let db = db.clone();
            async move {
                let result = async {
                    let job = db.get_agent_job(&job_id).await?.ok_or_else(|| {
                        anyhow::anyhow!("recovery export job {job_id} is missing")
                    })?;
                    export_job_csv_snapshot(db.clone(), &job).await
                }
                .await;
                let acknowledgment = db
                    .finish_agent_job_recovery_export(&job_id, &token, result.is_ok())
                    .await;
                result
                    .and(acknowledgment)
                    .map_err(|error| anyhow::anyhow!("{job_id}: {error}"))
            }
        })
        .buffer_unordered(4);
    while let Some(result) = exports.next().await {
        if let Err(error) = result {
            export_errors.push(error.to_string());
        }
    }
    if export_errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "failed to export reconciled agent job snapshots: {}",
            export_errors.join("; ")
        ))
    }
}

pub(crate) fn start_orphaned_agent_job_reconciler(db: Arc<codex_state::StateRuntime>) {
    let runner_instance_id = db.agent_job_runner_instance_id().to_string();
    {
        let mut active = ACTIVE_AGENT_JOB_RECONCILERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !active.insert(runner_instance_id.clone()) {
            return;
        }
    }
    let weak_db = Arc::downgrade(&db);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(ORPHANED_AGENT_JOB_RECONCILIATION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let Some(db) = weak_db.upgrade() else {
                break;
            };
            if db.is_closed() {
                break;
            }
            if let Err(error) = reconcile_orphaned_agent_jobs_after_restart(db).await {
                tracing::warn!(%error, "failed to reconcile expired agent job runner leases");
            }
        }
        ACTIVE_AGENT_JOB_RECONCILERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(runner_instance_id.as_str());
    });
}

async fn write_job_csv_atomically(output_path: PathBuf, csv_content: String) -> anyhow::Result<()> {
    // The publisher resolves symlinks before acquiring its destination lock.
    // Do not resolve again here and accidentally write outside that fence.
    tokio::task::spawn_blocking(move || {
        codex_file_system::write_atomically(&output_path, &csv_content)
    })
    .await
    .map_err(|err| anyhow::anyhow!("atomic csv write task failed: {err}"))??;
    Ok(())
}

async fn recover_running_items(
    session: Arc<Session>,
    db: Arc<codex_state::StateRuntime>,
    job_id: &str,
    active_items: &mut HashMap<ThreadId, ActiveJobItem>,
    runtime_timeout: Duration,
) -> anyhow::Result<()> {
    let running_items = db
        .list_agent_job_items(
            job_id,
            Some(codex_state::AgentJobItemStatus::Running),
            /*limit*/ None,
        )
        .await?;
    let mut finished = Vec::new();
    for item in running_items {
        let Some(assigned_thread_id) = item.assigned_thread_id.clone() else {
            db.settle_agent_job_item_for_thread(
                job_id,
                item.item_id.as_str(),
                None,
                "running item is missing assigned_thread_id",
            )
            .await?;
            continue;
        };
        let thread_id = match ThreadId::from_string(assigned_thread_id.as_str()) {
            Ok(thread_id) => thread_id,
            Err(err) => {
                let error_message = format!("invalid assigned_thread_id: {err:?}");
                db.settle_agent_job_item_for_thread(
                    job_id,
                    item.item_id.as_str(),
                    Some(assigned_thread_id.as_str()),
                    error_message.as_str(),
                )
                .await?;
                continue;
            }
        };
        // Keep the job/thread association available to the caller's error cleanup.
        active_items.insert(
            thread_id,
            ActiveJobItem {
                item_id: item.item_id.clone(),
                started_at: started_at_from_item(&item),
                status_rx: session
                    .services
                    .agent_control
                    .subscribe_status(thread_id)
                    .await
                    .ok(),
            },
        );
        if !is_item_stale(&item, runtime_timeout)
            && is_final(&session.services.agent_control.get_status(thread_id).await)
        {
            finished.push((thread_id, item.item_id));
        }
    }
    reap_stale_active_items(
        session.clone(),
        db.clone(),
        job_id,
        active_items,
        runtime_timeout,
    )
    .await?;
    settle_active_job_workers(
        session,
        db,
        job_id,
        active_items,
        finished,
        "worker finished without calling report_agent_job_result",
    )
    .await?;

    Ok(())
}

async fn shutdown_agent_job_worker(session: &Session, thread_id: ThreadId) -> anyhow::Result<()> {
    // A settled bulk worker is logically closed: retire its persisted parent
    // edge with the tree shutdown so root resume never restores it as a cold agent.
    match session
        .services
        .agent_control
        .close_agent(thread_id)
        .await
    {
        Ok(_) | Err(CodexErr::ThreadNotFound(_)) => Ok(()),
        Err(error) => Err(anyhow::anyhow!(
            "worker {thread_id} cleanup failed: {error}"
        )),
    }
}

async fn terminate_agent_job_workers(
    session: Arc<Session>,
    db: Arc<codex_state::StateRuntime>,
    job_id: &str,
    active_items: &mut HashMap<ThreadId, ActiveJobItem>,
    reason: &str,
) -> anyhow::Result<()> {
    let mut errors = Vec::new();
    let mut workers = active_items
        .iter()
        .map(|(thread, item)| (*thread, item.item_id.clone()))
        .collect::<HashMap<_, _>>();
    // A revoked runner may stop its retained local workers, not a successor's.
    let owned = match db.agent_job_is_owned(job_id).await {
        Ok(owned) => owned,
        Err(error) => {
            errors.push(error.to_string());
            false
        }
    };
    if owned {
        match db
            .list_agent_job_items(job_id, Some(codex_state::AgentJobItemStatus::Running), None)
            .await
        {
            Ok(items) => {
                for item in items {
                    match item
                        .assigned_thread_id
                        .as_deref()
                        .map(ThreadId::from_string)
                    {
                        Some(Ok(thread)) => {
                            workers.insert(thread, item.item_id);
                        }
                        _ => {
                            if let Err(error) = db
                                .settle_agent_job_item_for_thread(
                                    job_id,
                                    &item.item_id,
                                    item.assigned_thread_id.as_deref(),
                                    reason,
                                )
                                .await
                            {
                                errors.push(error.to_string());
                            }
                        }
                    }
                }
            }
            Err(error) => errors.push(format!("failed to load running items: {error}")),
        }
    }
    if let Err(error) = settle_active_job_workers(
        session,
        db,
        job_id,
        active_items,
        workers.into_iter().collect(),
        reason,
    )
    .await
    {
        errors.push(error.to_string());
    }
    if !errors.is_empty() {
        anyhow::bail!(errors.join("; "));
    }
    Ok(())
}

async fn settle_active_job_workers(
    session: Arc<Session>,
    db: Arc<codex_state::StateRuntime>,
    job_id: &str,
    active_items: &mut HashMap<ThreadId, ActiveJobItem>,
    workers: Vec<(ThreadId, String)>,
    reason: &str,
) -> anyhow::Result<()> {
    let mut errors = Vec::new();
    // Register the entire set before awaiting acknowledgments. One blocked
    // shutdown must not abandon the unvisited workers when an observer ends.
    let mut cleanup = futures::stream::iter(workers)
        .map(|(thread, item)| {
            let session = session.clone();
            let db = db.clone();
            async move {
                let result = async {
                    shutdown_agent_job_worker(&session, thread).await?;
                    db.settle_agent_job_item_for_thread(
                        job_id,
                        &item,
                        Some(thread.to_string().as_str()),
                        reason,
                    )
                    .await?;
                    anyhow::Ok(())
                }
                .await;
                (thread, item, result)
            }
        })
        .buffer_unordered(8);
    while let Some((thread, item, result)) = cleanup.next().await {
        match result {
            Ok(()) => {
                active_items.remove(&thread);
            }
            Err(error) => {
                active_items.entry(thread).or_insert(ActiveJobItem {
                    item_id: item,
                    started_at: Instant::now(),
                    status_rx: None,
                });
                errors.push(error.to_string());
            }
        }
    }
    if !errors.is_empty() {
        anyhow::bail!(errors.join("; "));
    }
    Ok(())
}

async fn find_finished_threads(
    session: Arc<Session>,
    active_items: &mut HashMap<ThreadId, ActiveJobItem>,
) -> Vec<(ThreadId, String)> {
    let mut finished = Vec::new();
    for (thread_id, item) in active_items.iter_mut() {
        let status = active_item_status(session.as_ref(), *thread_id, item).await;
        if is_final(&status) {
            finished.push((*thread_id, item.item_id.clone()));
        }
    }
    finished
}

async fn active_item_status(
    session: &Session,
    thread_id: ThreadId,
    item: &mut ActiveJobItem,
) -> AgentStatus {
    if let Some(status) = active_item_watch_status(item) {
        return status;
    }
    session.services.agent_control.get_status(thread_id).await
}

fn active_item_watch_status(item: &mut ActiveJobItem) -> Option<AgentStatus> {
    let status_rx = item.status_rx.as_mut()?;
    if status_rx.has_changed().is_err() {
        return None;
    }
    Some(status_rx.borrow_and_update().clone())
}

async fn wait_for_status_change(
    active_items: &HashMap<ThreadId, ActiveJobItem>,
    job_changes: &mut Receiver<u64>,
    capacity_changes: &mut Receiver<u64>,
    cancellation: &CancellationToken,
    runtime_timeout: Duration,
) {
    let mut waiters = FuturesUnordered::new();
    // Reconcile durable changes from other processes on the existing orphan
    // reconciliation cadence. Local commits and capacity releases wake directly.
    let mut deadline = Instant::now() + ORPHANED_AGENT_JOB_RECONCILIATION_INTERVAL;
    for item in active_items.values() {
        // Persisted timeouts can exceed Instant's platform-specific range.
        // Keep reconciliation and notification wakeups when no deadline fits.
        if let Some(runtime_deadline) = item.started_at.checked_add(runtime_timeout) {
            deadline = deadline.min(runtime_deadline);
        }
        if let Some(status_rx) = item.status_rx.as_ref()
            && status_rx.has_changed().is_ok()
        {
            let mut status_rx = status_rx.clone();
            waiters.push(async move {
                let _ = status_rx.changed().await;
            });
        } else {
            // Recovery for workers without a live subscription remains bounded.
            deadline = deadline.min(Instant::now() + MISSING_STATUS_RECHECK_INTERVAL);
        }
    }
    tokio::select! {
        _ = cancellation.cancelled() => {},
        _ = job_changes.changed() => {},
        _ = capacity_changes.changed() => {},
        _ = waiters.next(), if !waiters.is_empty() => {},
        _ = tokio::time::sleep_until(deadline) => {},
    }
}

/// Services existing workers while another worker's spawn is in flight.
/// Returns `false` once the job must stop instead of spawning.
async fn service_active_items_during_spawn(
    session: Arc<Session>,
    db: Arc<codex_state::StateRuntime>,
    job_id: &str,
    cancellation_token: &CancellationToken,
    active_items: &mut HashMap<ThreadId, ActiveJobItem>,
    runtime_timeout: Duration,
) -> anyhow::Result<bool> {
    if observe_parent_job_cancellation(cancellation_token, db.as_ref(), job_id).await?
        || !db.agent_job_is_owned_running(job_id).await?
    {
        return Ok(false);
    }
    reap_stale_active_items(
        session.clone(),
        db.clone(),
        job_id,
        active_items,
        runtime_timeout,
    )
    .await?;
    let finished = find_finished_threads(session.clone(), active_items).await;
    settle_active_job_workers(
        session,
        db,
        job_id,
        active_items,
        finished,
        "worker finished without calling report_agent_job_result",
    )
    .await?;
    Ok(true)
}

async fn reap_stale_active_items(
    session: Arc<Session>,
    db: Arc<codex_state::StateRuntime>,
    job_id: &str,
    active_items: &mut HashMap<ThreadId, ActiveJobItem>,
    runtime_timeout: Duration,
) -> anyhow::Result<bool> {
    let mut stale = Vec::new();
    for (thread_id, item) in active_items.iter() {
        if item.started_at.elapsed() >= runtime_timeout {
            stale.push((*thread_id, item.item_id.clone()));
        }
    }
    if stale.is_empty() {
        return Ok(false);
    }
    settle_active_job_workers(
        session,
        db,
        job_id,
        active_items,
        stale,
        &format!("worker exceeded max runtime of {runtime_timeout:?}"),
    )
    .await?;
    Ok(true)
}

#[cfg(test)]
async fn finalize_finished_item(
    session: Arc<Session>,
    db: Arc<codex_state::StateRuntime>,
    job_id: &str,
    item_id: &str,
    thread_id: ThreadId,
) -> anyhow::Result<()> {
    shutdown_agent_job_worker(&session, thread_id).await?;
    db.settle_agent_job_item_for_thread(
        job_id,
        item_id,
        Some(thread_id.to_string().as_str()),
        "worker finished without calling report_agent_job_result",
    )
    .await?;
    Ok(())
}

fn build_worker_prompt(
    job: &codex_state::AgentJob,
    item: &codex_state::AgentJobItem,
) -> anyhow::Result<String> {
    let job_id = job.id.as_str();
    let item_id = item.item_id.as_str();
    let instruction = render_instruction_template(job.instruction.as_str(), &item.row_json);
    let output_schema = job
        .output_schema_json
        .as_ref()
        .map(serde_json::to_string_pretty)
        .transpose()?
        .unwrap_or_else(|| "{}".to_string());
    let row_json = serde_json::to_string_pretty(&item.row_json)?;
    Ok(format!(
        "You are processing one item for a generic agent job.\n\
Job ID: {job_id}\n\
Item ID: {item_id}\n\n\
Task instruction:\n\
{instruction}\n\n\
Input row (JSON):\n\
{row_json}\n\n\
Expected result schema (JSON Schema or {{}}):\n\
{output_schema}\n\n\
You MUST record the result with `report_agent_job_result`; only `accepted: true` confirms this call recorded the item. Call it with:\n\
1. `job_id` = \"{job_id}\"\n\
2. `item_id` = \"{item_id}\"\n\
3. `result` = a JSON object that contains your analysis result for this row.\n\n\
If schema validation rejects your result, correct the payload and call it again. A returned `accepted: false` did not record the result; follow its rejection reason. Do not claim success or automatically replay after cancellation or loss of assignment.\n\n\
If you need to stop the job early, include `stop` = true in the tool call.\n\n\
After `accepted: true`, stop. If submission remains blocked, explain the blocker without claiming the result was recorded.",
    ))
}

fn render_instruction_template(instruction: &str, row_json: &Value) -> String {
    let row = row_json.as_object();
    let mut rendered = String::with_capacity(instruction.len());
    let mut cursor = 0;

    while cursor < instruction.len() {
        let remaining = &instruction[cursor..];
        if remaining.starts_with("{{") {
            rendered.push('{');
            cursor += 2;
            continue;
        }
        if remaining.starts_with("}}") {
            rendered.push('}');
            cursor += 2;
            continue;
        }
        if remaining.starts_with('{')
            && let Some(close_offset) = remaining[1..].find('}')
        {
            let key = &remaining[1..close_offset + 1];
            if !key.contains('{')
                && let Some(value) = row.and_then(|row| row.get(key))
            {
                if let Some(value) = value.as_str() {
                    rendered.push_str(value);
                } else {
                    rendered.push_str(&value.to_string());
                }
                cursor += close_offset + 2;
                continue;
            }
        }

        let Some(character) = remaining.chars().next() else {
            break;
        };
        rendered.push(character);
        cursor += character.len_utf8();
    }
    rendered
}

fn ensure_unique_headers(headers: &[String]) -> Result<(), FunctionCallError> {
    let mut seen = HashSet::new();
    for header in headers {
        if !seen.insert(header) {
            return Err(FunctionCallError::RespondToModel(format!(
                "csv header {header} is duplicated"
            )));
        }
        if AGENT_JOB_OUTPUT_COLUMNS.contains(&header.as_str()) {
            return Err(FunctionCallError::RespondToModel(format!(
                "csv header {header} conflicts with a generated output column"
            )));
        }
    }
    Ok(())
}

fn job_runtime_timeout(job: &codex_state::AgentJob) -> Duration {
    job.max_runtime_seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_AGENT_JOB_ITEM_TIMEOUT)
}

fn started_at_from_item(item: &codex_state::AgentJobItem) -> Instant {
    let now = chrono::Utc::now();
    let age = now.signed_duration_since(item.updated_at);
    if let Ok(age) = age.to_std() {
        Instant::now().checked_sub(age).unwrap_or_else(Instant::now)
    } else {
        Instant::now()
    }
}

fn is_item_stale(item: &codex_state::AgentJobItem, runtime_timeout: Duration) -> bool {
    let now = chrono::Utc::now();
    if let Ok(age) = now.signed_duration_since(item.updated_at).to_std() {
        age >= runtime_timeout
    } else {
        false
    }
}

fn default_output_csv_path(input_csv_path: &AbsolutePathBuf, job_id: &str) -> AbsolutePathBuf {
    let stem = input_csv_path
        .as_path()
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("agent_job_output");
    // Leave room for the complete identity, extension and atomic-writer lock
    // suffix on filesystems with a 255-byte component limit.
    let mut stem_end = stem.len().min(160);
    while !stem.is_char_boundary(stem_end) {
        stem_end -= 1;
    }
    let stem = &stem[..stem_end];
    let output_dir = input_csv_path
        .parent()
        .unwrap_or_else(|| input_csv_path.clone());
    output_dir.join(format!("{stem}.agent-job-{job_id}.csv"))
}

fn parse_csv(content: &str) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(content.as_bytes());
    let headers_record = reader.headers().map_err(|err| err.to_string())?;
    let mut headers: Vec<String> = headers_record.iter().map(str::to_string).collect();
    if let Some(first) = headers.first_mut() {
        *first = first.trim_start_matches('\u{feff}').to_string();
    }
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|err| err.to_string())?;
        let row: Vec<String> = record.iter().map(str::to_string).collect();
        rows.push(row);
    }
    Ok((headers, rows))
}

#[derive(Default)]
struct JobItemIds {
    seen: HashSet<String>,
    next_suffix: HashMap<String, usize>,
    #[cfg(test)]
    probes: usize,
}

impl JobItemIds {
    fn insert(&mut self, id: String) -> bool {
        #[cfg(test)]
        {
            self.probes += 1;
        }
        self.seen.insert(id)
    }

    fn allocate(&mut self, base: String) -> Result<String, FunctionCallError> {
        if self.insert(base.clone()) {
            return Ok(base);
        }
        // Unique inputs never allocate a suffix-cache entry. The global set
        // still checks collisions with literal IDs such as x-2 and x-2-2.
        let mut suffix = self.next_suffix.get(&base).copied().unwrap_or(2);
        loop {
            let candidate = format!("{base}-{suffix}");
            let inserted = self.insert(candidate.clone());
            let next = suffix.checked_add(1).ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "csv item ID suffix space exhausted for {base}"
                ))
            })?;
            if inserted {
                self.next_suffix.insert(base, next);
                return Ok(candidate);
            }
            suffix = next;
        }
    }
}

fn render_job_csv(
    headers: &[String],
    items: &[codex_state::AgentJobItem],
) -> Result<String, FunctionCallError> {
    let mut csv = String::new();
    let mut output_headers = headers.to_vec();
    output_headers.extend(AGENT_JOB_OUTPUT_COLUMNS.map(str::to_string));
    csv.push_str(
        output_headers
            .iter()
            .map(|header| csv_escape(header.as_str()))
            .collect::<Vec<_>>()
            .join(",")
            .as_str(),
    );
    csv.push('\n');
    for item in items {
        let row_object = item.row_json.as_object().ok_or_else(|| {
            let item_id = item.item_id.as_str();
            FunctionCallError::RespondToModel(format!(
                "row_json for item {item_id} is not a JSON object"
            ))
        })?;
        let mut row_values = Vec::new();
        for header in headers {
            let value = row_object
                .get(header)
                .map_or_else(String::new, value_to_csv_string);
            row_values.push(csv_escape(value.as_str()));
        }
        row_values.push(csv_escape(item.job_id.as_str()));
        row_values.push(csv_escape(item.item_id.as_str()));
        row_values.push(csv_escape(item.row_index.to_string().as_str()));
        row_values.push(csv_escape(
            item.source_id.clone().unwrap_or_default().as_str(),
        ));
        row_values.push(csv_escape(item.status.as_str()));
        row_values.push(csv_escape(item.attempt_count.to_string().as_str()));
        row_values.push(csv_escape(
            item.last_error.clone().unwrap_or_default().as_str(),
        ));
        row_values.push(csv_escape(
            item.result_json
                .as_ref()
                .map_or_else(String::new, std::string::ToString::to_string)
                .as_str(),
        ));
        row_values.push(csv_escape(
            item.reported_at
                .map(|value| value.to_rfc3339())
                .unwrap_or_default()
                .as_str(),
        ));
        row_values.push(csv_escape(
            item.completed_at
                .map(|value| value.to_rfc3339())
                .unwrap_or_default()
                .as_str(),
        ));
        csv.push_str(row_values.join(",").as_str());
        csv.push('\n');
    }
    Ok(csv)
}

fn value_to_csv_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('\n') || value.contains('\r') || value.contains('"') {
        let escaped = value.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
#[path = "agent_jobs_tests.rs"]
mod tests;
