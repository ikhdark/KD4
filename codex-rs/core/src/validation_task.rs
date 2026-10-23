//! Full-suite grants belong to accepted tasks, never inferred from prose/turns.
//!
//! Root clients may submit Application additional_context under
//! `codex.validation_task.v1` with `{"task_id":"...","grant_id":"..."}`.
//! Omission continues the existing task; reusing IDs never replenishes it.
//! Children inherit their parent's captured scope and cannot mint grants.

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::sandboxing::ToolCtx;
use codex_command_analysis::command_preflight::owned_suite_nonexecution;
use std::collections::HashMap;
use std::path::Path;

pub(crate) const METADATA_KEY: &str = "codex.validation_task.v1";

#[derive(Default)]
struct AdmissionRefusals(std::sync::Mutex<HashMap<String, bool>>);

/// `validation_proof` is the launch owner's repository-runner-aware proof
/// capability. Settlement cannot recover it: call-argument classification has
/// no runner manifest, so a refused `just`/runner validation would otherwise
/// vanish. `false` defers to that call-argument classification.
pub(crate) fn record_admission_refusal(ctx: &ToolCtx, validation_proof: bool) {
    ctx.turn
        .extension_data
        .get_or_init(AdmissionRefusals::default)
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(ctx.call_id.clone(), validation_proof);
}

/// `Some(validation_proof)` when this call was refused before launch.
pub(crate) fn take_admission_refusal(turn: &TurnContext, call: &str) -> Option<bool> {
    turn.extension_data
        .get::<AdmissionRefusals>()
        .and_then(|refusals| {
            refusals
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(call)
        })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskGrant {
    task_id: String,
    grant_id: String,
}

pub(crate) fn reservation_owner(session: &Session, turn: &TurnContext, call: &str) -> String {
    format!("{}:{}:{call}", session.thread_id(), turn.sub_id)
}

pub(crate) async fn accept_metadata(
    session: &Session,
    entry: &codex_protocol::protocol::AdditionalContextEntry,
    root: bool,
) -> Result<(), String> {
    if entry.kind != codex_protocol::protocol::AdditionalContextKind::Application || !root {
        return Err(
            "validation task grants require accepted root-client application metadata".into(),
        );
    }
    let grant: TaskGrant = serde_json::from_str(&entry.value).map_err(|error| error.to_string())?;
    for id in [&grant.task_id, &grant.grant_id] {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
        {
            return Err(
                "validation task and grant IDs must be nonempty bounded identifiers".into(),
            );
        }
    }
    let store = session
        .services
        .agent_control
        .task_coordinator()
        .store()
        .ok_or("validation task persistence is unavailable")?;
    // Qualify caller IDs by the durable root-session identity, not a repository.
    let task = format!("{}:{}", session.session_id(), grant.task_id);
    store
        .accept_validation_task_grant(&session.thread_id().to_string(), &task, &grant.grant_id)
        .await
        .map_err(|error| error.to_string())
}

/// The owner's acknowledgement channel for one local full-suite launch.
pub(crate) struct FullSuiteDispatch {
    path: std::path::PathBuf,
    attempt: String,
}

impl FullSuiteDispatch {
    /// Only for a launch whose process creation definitely failed: no child
    /// exists to own this record, so the next reservation may refund the
    /// attempt. First write wins; an existing record is never replaced.
    pub(crate) async fn acknowledge_not_spawned(self) {
        use tokio::io::AsyncWriteExt as _;

        let record = serde_json::json!({
            "version": 1,
            "attempt": self.attempt,
            "status": "not_started",
        })
        .to_string();
        let Ok(mut file) = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
            .await
        else {
            return;
        };
        // A partial record does not parse, so a failed write stays spent.
        if file.write_all(record.as_bytes()).await.is_ok() {
            let _ = file.sync_all().await;
        }
    }
}

/// Called only after owner preflight/admission and immediately before possible
/// spawn. No Rust lock is held across storage; a lost/uncertain commit fails closed.
/// Returns the local acknowledgement channel when this call started a reserved suite.
pub(crate) async fn begin_dispatch(
    ctx: &ToolCtx,
    env: &mut HashMap<String, String>,
    local: bool,
) -> Result<Option<FullSuiteDispatch>, String> {
    let owner = reservation_owner(&ctx.session, &ctx.turn, &ctx.call_id);
    let budget = &ctx.turn.full_suite_budget;
    if budget.started_for(&owner) {
        return Err("full-suite launch blocked: this tool call already crossed the possible-start boundary; sandbox retries cannot create another suite".into());
    }
    if !budget.reserved_for(&owner) {
        return Ok(None);
    }
    let Some((task, grant)) = budget.task_scope() else {
        // isolated, nonpersistent test contexts
        return budget.start(&owner, None).map(|()| None);
    };
    let store = ctx
        .session
        .services
        .agent_control
        .task_coordinator()
        .store()
        .ok_or("full-suite launch blocked: durable task budget is unavailable")?;
    let previous = store
        .validation_suite_attempt(task, grant)
        .await
        .map_err(|error| error.to_string())?;
    let refunded = tokio::task::spawn_blocking(move || {
        previous
            .filter(|(attempt, path)| owned_suite_nonexecution(Path::new(path), attempt))
            .map(|(attempt, _)| attempt)
    })
    .await
    .map_err(|error| error.to_string())?;
    let attempt = uuid::Uuid::new_v4().to_string();
    let directory = ctx.turn.config.codex_home.join("tmp/full-suite-dispatch");
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|error| error.to_string())?;
    let path = directory.join(format!("{attempt}.json"));
    if !store
        .reserve_validation_suite(
            task,
            grant,
            &attempt,
            &path.to_string_lossy(),
            refunded.as_deref(),
        )
        .await
        .map_err(|error| error.to_string())?
    {
        return Err("Full-suite launch blocked: this task/grant already has a reserved, started, or uncertain run; another full suite requires explicit user permission.".into());
    }
    if !store
        .start_validation_suite(task, grant, &attempt)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err("full-suite launch blocked: task reservation ownership changed".into());
    }
    budget.start(&owner, local.then(|| (path.to_path_buf(), attempt.clone())))?;
    // Remote/opaque starts stay spent when their dispatch cannot be observed.
    if !local {
        return Ok(None);
    }
    env.insert(
        "CODEX_FULL_SUITE_ACK_PATH".into(),
        path.to_string_lossy().into_owned(),
    );
    env.insert("CODEX_FULL_SUITE_ATTEMPT".into(), attempt.clone());
    Ok(Some(FullSuiteDispatch { path: path.to_path_buf(), attempt }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unspawned_dispatch_acknowledges_nonexecution_without_replacing_owner_records() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attempt.json");
        FullSuiteDispatch {
            path: path.clone(),
            attempt: "nonce".into(),
        }
        .acknowledge_not_spawned()
        .await;
        assert!(owned_suite_nonexecution(&path, "nonce"));
        assert!(!owned_suite_nonexecution(&path, "other"));

        // A child that reached its dispatch boundary first keeps the suite spent.
        let started = root.path().join("started.json");
        std::fs::write(
            &started,
            r#"{"version":1,"attempt":"nonce","status":"started"}"#,
        )
        .unwrap();
        FullSuiteDispatch {
            path: started.clone(),
            attempt: "nonce".into(),
        }
        .acknowledge_not_spawned()
        .await;
        assert!(!owned_suite_nonexecution(&started, "nonce"));
    }
}
