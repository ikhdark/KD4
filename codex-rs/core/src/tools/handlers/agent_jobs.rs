use crate::agent::control::AgentJobBinding;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::role::AgentRoleModelLocks;
use crate::agent::status::is_final;
use crate::config::Config;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::multi_agents::apply_spawn_agent_model_defaults_and_overrides;
use crate::tools::handlers::multi_agents::apply_spawn_agent_service_tier;
use crate::tools::handlers::multi_agents::build_agent_spawn_config;
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
use tokio::sync::watch::Receiver;
use tokio::time::Duration;
use tokio::time::Instant;
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
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_AGENT_JOB_ITEM_TIMEOUT: Duration = Duration::from_secs(60 * 30);
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
}

#[derive(Debug, Deserialize)]
struct ReportAgentJobResultArgs {
    job_id: String,
    item_id: String,
    result: Value,
    stop: Option<bool>,
}

#[derive(Debug, Serialize)]
struct SpawnAgentsOnCsvResult {
    job_id: String,
    status: String,
    output_csv_path: String,
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
}

#[derive(Debug, Clone)]
struct JobRunnerOptions {
    max_concurrency: usize,
    spawn_config: Config,
}

#[derive(Debug, Clone)]
struct ActiveJobItem {
    item_id: String,
    started_at: Instant,
    status_rx: Option<Receiver<AgentStatus>>,
}

fn required_state_db(
    session: &Arc<Session>,
) -> Result<Arc<codex_state::StateRuntime>, FunctionCallError> {
    session.state_db().ok_or_else(|| {
        FunctionCallError::Fatal("sqlite state db is unavailable for this session".to_string())
    })
}

async fn build_runner_options(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    requested_concurrency: Option<usize>,
) -> Result<JobRunnerOptions, FunctionCallError> {
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
    let base_instructions = session.get_base_instructions().await;
    let mut spawn_config = build_agent_spawn_config(&base_instructions, turn.as_ref())?;
    apply_spawn_agent_model_defaults_and_overrides(
        session,
        turn.as_ref(),
        &mut spawn_config,
        /*requested_model*/ None,
        /*requested_reasoning_effort*/ None,
        AgentRoleModelLocks::default(),
    )
    .await?;
    apply_spawn_agent_service_tier(
        session,
        turn.as_ref(),
        &mut spawn_config,
        turn.config.service_tier.as_deref(),
        /*requested_service_tier*/ None,
    )
    .await?;
    Ok(JobRunnerOptions {
        max_concurrency,
        spawn_config,
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
    turn: Arc<TurnContext>,
    db: Arc<codex_state::StateRuntime>,
    job_id: String,
    options: JobRunnerOptions,
    cancellation_token: CancellationToken,
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
    let mut active_items: HashMap<ThreadId, ActiveJobItem> = HashMap::new();
    recover_running_items(
        session.clone(),
        db.clone(),
        job_id.as_str(),
        &mut active_items,
        runtime_timeout,
    )
    .await?;

    let mut cancel_requested = db.is_agent_job_cancelled(job_id.as_str()).await?;
    loop {
        let mut progressed = false;

        if observe_parent_job_cancellation(&cancellation_token, db.as_ref(), job_id.as_str())
            .await?
        {
            cancel_requested = true;
        }

        if !cancel_requested && db.is_agent_job_cancelled(job_id.as_str()).await? {
            cancel_requested = true;
        }

        if cancel_requested {
            break;
        }

        if !cancel_requested && active_items.len() < options.max_concurrency {
            let slots = options.max_concurrency - active_items.len();
            let pending_items = db
                .list_agent_job_items(
                    job_id.as_str(),
                    Some(codex_state::AgentJobItemStatus::Pending),
                    Some(slots),
                )
                .await?;
            for item in pending_items {
                if observe_parent_job_cancellation(
                    &cancellation_token,
                    db.as_ref(),
                    job_id.as_str(),
                )
                .await?
                {
                    cancel_requested = true;
                    break;
                }
                if db.is_agent_job_cancelled(job_id.as_str()).await? {
                    cancel_requested = true;
                    break;
                }
                let prompt = build_worker_prompt(&job, &item)?;
                let items = vec![UserInput::Text {
                    text: prompt,
                    text_elements: Vec::new(),
                }];
                let thread_id = match session
                    .services
                    .agent_control
                    .spawn_agent_with_metadata(
                        options.spawn_config.clone(),
                        items,
                        Some(SessionSource::SubAgent(SubAgentSource::Other(format!(
                            "agent_job:{job_id}"
                        )))),
                        SpawnAgentOptions {
                            parent_thread_id: Some(session.thread_id),
                            environments: Some(turn.environments.to_selections()),
                            agent_job_binding: Some(AgentJobBinding {
                                state_db: db.clone(),
                                job_id: job_id.clone(),
                                item_id: item.item_id.clone(),
                            }),
                            ..Default::default()
                        },
                    )
                    .await
                {
                    Ok(spawned_agent) => spawned_agent.thread_id,
                    Err(CodexErr::AgentLimitReached { .. }) => {
                        if observe_parent_job_cancellation(
                            &cancellation_token,
                            db.as_ref(),
                            job_id.as_str(),
                        )
                        .await?
                        {
                            cancel_requested = true;
                            break;
                        }
                        break;
                    }
                    Err(err) => {
                        let error_message = format!("failed to spawn worker: {err}");
                        let job_cancelled = observe_parent_job_cancellation(
                            &cancellation_token,
                            db.as_ref(),
                            job_id.as_str(),
                        )
                        .await?
                            || db.is_agent_job_cancelled(job_id.as_str()).await?;
                        if job_cancelled {
                            cancel_requested = true;
                            break;
                        }
                        let marked_failed = db
                            .mark_agent_job_item_spawn_failed(
                                job_id.as_str(),
                                item.item_id.as_str(),
                                error_message.as_str(),
                            )
                            .await?;
                        if !marked_failed {
                            if db.is_agent_job_cancelled(job_id.as_str()).await? {
                                cancel_requested = true;
                                break;
                            }
                            return Err(anyhow::anyhow!(
                                "{error_message}; item was no longer pending or bound to the worker"
                            ));
                        }
                        progressed = true;
                        continue;
                    }
                };
                active_items.insert(
                    thread_id,
                    ActiveJobItem {
                        item_id: item.item_id.clone(),
                        started_at: Instant::now(),
                        status_rx: session
                            .services
                            .agent_control
                            .subscribe_status(thread_id)
                            .await
                            .ok(),
                    },
                );
                progressed = true;
                if observe_parent_job_cancellation(
                    &cancellation_token,
                    db.as_ref(),
                    job_id.as_str(),
                )
                .await?
                    || db.is_agent_job_cancelled(job_id.as_str()).await?
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
            &mut active_items,
            runtime_timeout,
        )
        .await?
        {
            progressed = true;
        }

        let finished = find_finished_threads(session.clone(), &mut active_items).await;
        if finished.is_empty() {
            let progress = db.get_agent_job_progress(job_id.as_str()).await?;
            if progress.pending_items == 0 && progress.running_items == 0 && active_items.is_empty()
            {
                break;
            }
            if !progressed {
                wait_for_status_change(&active_items).await;
            }
            continue;
        }

        for (thread_id, item_id) in finished {
            finalize_finished_item(
                session.clone(),
                db.clone(),
                job_id.as_str(),
                item_id.as_str(),
                thread_id,
            )
            .await?;
            active_items.remove(&thread_id);
        }
    }

    if observe_parent_job_cancellation(&cancellation_token, db.as_ref(), job_id.as_str()).await? {
        cancel_requested = true;
    }
    if !cancel_requested && db.is_agent_job_cancelled(job_id.as_str()).await? {
        cancel_requested = true;
    }

    let cleanup_error = if cancel_requested {
        terminate_agent_job_workers(
            session.clone(),
            db.clone(),
            job_id.as_str(),
            &mut active_items,
            "job cancelled before worker completion",
        )
        .await
        .err()
    } else {
        None
    };
    let export_error = export_job_csv_snapshot(db.clone(), &job).await.err();
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
    let items = db
        .list_agent_job_items(job.id.as_str(), /*status*/ None, /*limit*/ None)
        .await?;
    let csv_content = render_job_csv(job.input_headers.as_slice(), items.as_slice())
        .map_err(|err| anyhow::anyhow!("failed to render job csv for auto-export: {err}"))?;
    let output_path = PathBuf::from(job.output_csv_path.clone());
    write_job_csv_atomically(output_path, csv_content).await?;
    Ok(())
}

async fn write_job_csv_atomically(output_path: PathBuf, csv_content: String) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        let write_paths = crate::path_utils::resolve_symlink_write_paths(&output_path)?;
        crate::path_utils::write_atomically(&write_paths.write_path, &csv_content)
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
    for item in running_items {
        if is_item_stale(&item, runtime_timeout) {
            let error_message = format!("worker exceeded max runtime of {runtime_timeout:?}");
            db.mark_agent_job_item_failed(job_id, item.item_id.as_str(), error_message.as_str())
                .await?;
            if let Some(assigned_thread_id) = item.assigned_thread_id.as_ref()
                && let Ok(thread_id) = ThreadId::from_string(assigned_thread_id.as_str())
            {
                let _ = session
                    .services
                    .agent_control
                    .shutdown_live_agent(thread_id)
                    .await;
            }
            continue;
        }
        let Some(assigned_thread_id) = item.assigned_thread_id.clone() else {
            db.mark_agent_job_item_failed(
                job_id,
                item.item_id.as_str(),
                "running item is missing assigned_thread_id",
            )
            .await?;
            continue;
        };
        let thread_id = match ThreadId::from_string(assigned_thread_id.as_str()) {
            Ok(thread_id) => thread_id,
            Err(err) => {
                let error_message = format!("invalid assigned_thread_id: {err:?}");
                db.mark_agent_job_item_failed(
                    job_id,
                    item.item_id.as_str(),
                    error_message.as_str(),
                )
                .await?;
                continue;
            }
        };
        if is_final(&session.services.agent_control.get_status(thread_id).await) {
            finalize_finished_item(
                session.clone(),
                db.clone(),
                job_id,
                item.item_id.as_str(),
                thread_id,
            )
            .await?;
        } else {
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
        }
    }
    Ok(())
}

async fn terminate_agent_job_workers(
    session: Arc<Session>,
    db: Arc<codex_state::StateRuntime>,
    job_id: &str,
    active_items: &mut HashMap<ThreadId, ActiveJobItem>,
    reason: &str,
) -> anyhow::Result<()> {
    let mut state_errors = Vec::new();
    let mut item_ids = HashSet::new();
    let mut thread_ids = HashSet::new();
    for (thread_id, item) in std::mem::take(active_items) {
        thread_ids.insert(thread_id);
        item_ids.insert(item.item_id);
    }

    match db
        .list_agent_job_items(
            job_id,
            Some(codex_state::AgentJobItemStatus::Running),
            /*limit*/ None,
        )
        .await
    {
        Ok(running_items) => {
            for item in running_items {
                let item_id = item.item_id;
                if let Some(assigned_thread_id) = item.assigned_thread_id {
                    match ThreadId::from_string(assigned_thread_id.as_str()) {
                        Ok(thread_id) => {
                            thread_ids.insert(thread_id);
                        }
                        Err(error) => {
                            tracing::warn!(
                                job_id,
                                item_id,
                                assigned_thread_id,
                                error = ?error,
                                "failed to parse worker thread id while terminating agent job"
                            );
                        }
                    }
                }
                item_ids.insert(item_id);
            }
        }
        Err(error) => state_errors.push(format!("failed to load running items: {error}")),
    }

    for item_id in item_ids {
        if let Err(err) = db
            .mark_agent_job_item_failed(job_id, item_id.as_str(), reason)
            .await
        {
            state_errors.push(format!("failed to terminate item {item_id}: {err}"));
        }
    }

    for thread_id in thread_ids {
        if let Err(err) = session
            .services
            .agent_control
            .shutdown_live_agent(thread_id)
            .await
        {
            tracing::warn!(
                %thread_id,
                error = %err,
                "failed to shut down worker for cancelled agent job"
            );
        }
    }
    if !state_errors.is_empty() {
        return Err(anyhow::anyhow!(state_errors.join("; ")));
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

async fn wait_for_status_change(active_items: &HashMap<ThreadId, ActiveJobItem>) {
    let mut waiters = FuturesUnordered::new();
    for item in active_items.values() {
        if let Some(status_rx) = item.status_rx.as_ref() {
            let mut status_rx = status_rx.clone();
            waiters.push(async move {
                let _ = status_rx.changed().await;
            });
        }
    }
    if waiters.is_empty() {
        tokio::time::sleep(STATUS_POLL_INTERVAL).await;
        return;
    }
    let _ = timeout(STATUS_POLL_INTERVAL, waiters.next()).await;
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
    for (thread_id, item_id) in stale {
        let error_message = format!("worker exceeded max runtime of {runtime_timeout:?}");
        db.mark_agent_job_item_failed(job_id, item_id.as_str(), error_message.as_str())
            .await?;
        let _ = session
            .services
            .agent_control
            .shutdown_live_agent(thread_id)
            .await;
        active_items.remove(&thread_id);
    }
    Ok(true)
}

async fn finalize_finished_item(
    session: Arc<Session>,
    db: Arc<codex_state::StateRuntime>,
    job_id: &str,
    item_id: &str,
    thread_id: ThreadId,
) -> anyhow::Result<()> {
    let item = db
        .get_agent_job_item(job_id, item_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("job item not found for finalization: {job_id}/{item_id}")
        })?;
    if matches!(item.status, codex_state::AgentJobItemStatus::Running) {
        if item.result_json.is_some() {
            let _ = db.mark_agent_job_item_completed(job_id, item_id).await?;
        } else {
            let _ = db
                .mark_agent_job_item_failed(
                    job_id,
                    item_id,
                    "worker finished without calling report_agent_job_result",
                )
                .await?;
        }
    }
    let _ = session
        .services
        .agent_control
        .shutdown_live_agent(thread_id)
        .await;
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
You MUST successfully call the `report_agent_job_result` tool with:\n\
1. `job_id` = \"{job_id}\"\n\
2. `item_id` = \"{item_id}\"\n\
3. `result` = a JSON object that contains your analysis result for this row.\n\n\
If the tool rejects your result, correct the payload and call it again.\n\n\
If you need to stop the job early, include `stop` = true in the tool call.\n\n\
After the tool call succeeds, stop.",
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
    let job_suffix = &job_id[..8];
    let output_dir = input_csv_path
        .parent()
        .unwrap_or_else(|| input_csv_path.clone());
    output_dir.join(format!("{stem}.agent-job-{job_suffix}.csv"))
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
        if row.iter().all(std::string::String::is_empty) {
            continue;
        }
        rows.push(row);
    }
    Ok((headers, rows))
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
