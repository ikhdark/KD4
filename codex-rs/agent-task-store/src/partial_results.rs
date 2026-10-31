//! Versioned provisional evidence in the existing task store. Publication and
//! its wake observation commit together; neither changes acceptance or seals work.

use super::*;
use serde::Deserialize;

const MAX_PARTIAL_RESULT_BYTES: usize = 64 * 1024;

/// Worker-declared identity, not host-verified acquisition or freshness proof.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PartialResultSource {
    pub identity: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PartialResultDraft {
    pub summary: String,
    /// Copy the evidence here, not just a worker-private artifact locator.
    pub evidence: String,
    pub sources: Vec<PartialResultSource>,
    pub limitations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<WorkerFinding>,
}

/// Original report data is immutable; grouping never replaces an occurrence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerFinding {
    pub id: String,
    pub defect: String,
    pub trigger: String,
    pub remedy: String,
    pub locations: Vec<String>,
    pub occurrence_count: u64,
    pub evidence: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PartialResultStatus {
    Provisional,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentPartialResult {
    pub schema_version: u32,
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub repository_id: String,
    pub workspace_id: String,
    pub version: u32,
    pub status: PartialResultStatus,
    pub result: PartialResultDraft,
    pub created_at: chrono::DateTime<Utc>,
}

impl PartialResultDraft {
    fn validate(&self) -> StoreResult<()> {
        let mut ids = HashSet::new();
        for finding in &self.findings {
            if [&finding.id, &finding.defect, &finding.trigger, &finding.remedy]
                .iter().any(|value| value.trim().is_empty())
                || !ids.insert(&finding.id) || finding.occurrence_count == 0
                || finding.locations.is_empty() || finding.evidence.is_empty()
                || finding.locations.iter().chain(&finding.evidence).any(|value| value.trim().is_empty())
            {
                return Err(StoreError::InvalidAssignment("findings require unique original IDs, explicit defect/trigger/remedy, locations, positive occurrence counts and distinct retained evidence".into()));
            }
        }
        if self.summary.trim().is_empty()
            || self.evidence.trim().is_empty()
            || self.sources.is_empty()
            || self.sources.len() > 128
            || self.limitations.iter().any(|value| value.trim().is_empty())
            || self.sources.iter().any(|source| {
                source.identity.trim().is_empty()
                    || source.sha256.len() != 64
                    || !source.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            || serde_json::to_vec(self)?.len() > MAX_PARTIAL_RESULT_BYTES
        {
            return Err(StoreError::InvalidAssignment(
                "partial result requires nonempty summary/evidence, 1-128 source identities with SHA-256 hashes, explicit limitations (empty allowed), and at most 64 KiB of JSON".into(),
            ));
        }
        Ok(())
    }
}

impl LocalAgentTaskStore {
    /// Compare-and-append under the existing writer lock. An identical replay
    /// of the immediately committed version is idempotent, including lost replies.
    pub async fn publish_agent_partial_result(
        &self,
        root_session_id: String,
        attempt_id: AttemptId,
        expected_version: u32,
        draft: PartialResultDraft,
    ) -> StoreResult<AgentPartialResult> {
        draft.validate()?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let attempt = require_active_current_attempt_tx(&mut transaction, attempt_id).await?;
        let assignment = load_assignment_tx(&mut transaction, attempt.assignment_id).await?;
        TaskActor::RootSession(root_session_id).require_root_for(&assignment.root_session_id)?;
        let latest = sqlx::query_scalar::<_, String>(
            "SELECT body_json FROM partial_results WHERE attempt_id = ? ORDER BY version DESC LIMIT 1",
        ).bind(attempt_id.to_string()).fetch_optional(&mut *transaction).await?
            .map(|body| decode::<AgentPartialResult>(&body)).transpose()?;
        let current_version = latest.as_ref().map_or(0, |result| result.version);
        let version = expected_version.checked_add(1).ok_or_else(|| {
            StoreError::InvalidAssignment("partial result version exhausted".into())
        })?;
        if current_version != expected_version {
            if let Some(latest) = latest
                && latest.version == version
                && latest.result == draft
            {
                return Ok(latest);
            }
            return Err(StoreError::InvalidAssignment(format!(
                "partial result version conflict: expected {expected_version}, current {current_version}; retrieve the retained result before publishing again",
            )));
        }
        let result = AgentPartialResult {
            schema_version: 1,
            assignment_id: assignment.assignment_id,
            attempt_id,
            repository_id: assignment.repository_id.clone(),
            workspace_id: assignment.workspace_id.clone(),
            version,
            status: PartialResultStatus::Provisional,
            result: draft,
            created_at: Utc::now(),
        };
        sqlx::query(
            "INSERT INTO partial_results (attempt_id, version, body_json) VALUES (?, ?, ?)",
        )
        .bind(attempt_id.to_string())
        .bind(i64::from(version))
        .bind(encode(&result)?)
        .execute(&mut *transaction)
        .await?;
        let workspace_id =
            assignment_workspace_id_tx(&mut transaction, assignment.assignment_id).await?;
        sqlx::query(
            "UPDATE workspace_actors SET last_progress_at = ?, state = 'active',
                 nudge_sent_at = NULL, lease_expires_at = ?
             WHERE workspace_id = ? AND attempt_id = ?",
        )
        .bind(encode(&result.created_at)?)
        .bind(encode(
            &(result.created_at
                + chrono::Duration::seconds(crate::DEFAULT_WORKSPACE_LEASE_SECONDS)),
        )?)
        .bind(workspace_id)
        .bind(attempt_id.to_string())
        .execute(&mut *transaction)
        .await?;
        append_observation_tx(
            &mut transaction, &assignment, attempt_id, ObservationKind::PartialResult,
            format!("Provisional partial result v{version}; retrieve with get_agent_partial_result assignment_id={} attempt_id={attempt_id} version={version}. This is not accepted evidence or terminal completion.", assignment.assignment_id),
            None,
        ).await?;
        transaction.commit().await?;
        self.notify_wake_waiters();
        Ok(result)
    }

    /// Read the latest or an exact immutable version, including historical
    /// attempts. Root scope and assignment/attempt identity share one snapshot.
    pub async fn get_agent_partial_result(
        &self,
        root_session_id: String,
        assignment_id: AssignmentId,
        attempt_id: Option<AttemptId>,
        version: Option<u32>,
    ) -> StoreResult<Option<AgentPartialResult>> {
        if version == Some(0) {
            return Err(StoreError::InvalidAssignment(
                "partial result versions start at 1".into(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        TaskActor::RootSession(root_session_id).require_root_for(&assignment.root_session_id)?;
        let attempt = match attempt_id {
            Some(id) => load_attempt_tx(&mut transaction, id).await?,
            None => load_current_attempt_tx(&mut transaction, assignment_id).await?,
        };
        if attempt.assignment_id != assignment_id {
            return Err(StoreError::InvalidAssignment(
                "partial result attempt belongs to another assignment".into(),
            ));
        }
        let body = match version {
            Some(version) => sqlx::query_scalar::<_, String>(
                "SELECT body_json FROM partial_results WHERE attempt_id = ? AND version = ?",
            ).bind(attempt.attempt_id.to_string()).bind(i64::from(version))
                .fetch_optional(&mut *transaction).await?,
            None => sqlx::query_scalar::<_, String>(
                "SELECT body_json FROM partial_results WHERE attempt_id = ? ORDER BY version DESC LIMIT 1",
            ).bind(attempt.attempt_id.to_string()).fetch_optional(&mut *transaction).await?,
        };
        transaction.commit().await?;
        body.map(|body| decode(&body)).transpose()
    }
}
