//! Advisory, attempt-scoped edit provenance. Declarations never gate mutation.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditIntent {
    pub path: String,
    /// Inclusive source lines at base_revision; None means the entire file.
    pub region: Option<EditIntentRegion>,
    pub base_revision: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditIntentRegion {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct EditIntentWarning {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub agent_path: Option<String>,
    pub thread_id: Option<String>,
    pub declared: EditIntent,
    pub competing: EditIntent,
    /// Different baselines have incomparable coordinates: warn conservatively.
    pub same_base_revision: bool,
}

impl LocalAgentTaskStore {
    /// Replace this attempt's declarations (empty retires them). Selection is
    /// serialized with declaration replacement, not with filesystem mutations.
    pub async fn declare_edit_intents(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        expected_attempt_id: AttemptId,
        intents: Vec<EditIntent>,
    ) -> StoreResult<Vec<EditIntentWarning>> {
        let context = validation_context(&self.pool, expected_attempt_id).await?;
        if context.assignment.assignment_id != assignment_id {
            return Err(StoreError::InvalidAssignment(
                "edit intent attempt belongs to another assignment".into(),
            ));
        }
        let root = context.repo_root;
        let intents = tokio::task::spawn_blocking(move || {
            if intents.len() > 128 {
                return Err(StoreError::InvalidAssignment("at most 128 edit intents per declaration".into()));
            }
            intents.into_iter().map(|mut intent| {
                intent.path = crate::normalize_repo_path(&root, &intent.path)?;
                if intent.base_revision.trim().is_empty() || intent.base_revision.len() > 256
                    || intent.base_revision.contains(['\0', '\r', '\n'])
                    || intent.region.as_ref().is_some_and(|r| r.start == 0 || r.end < r.start)
                {
                    return Err(StoreError::InvalidAssignment("edit intent needs a bounded nonempty base revision and a valid inclusive region".into()));
                }
                Ok(intent)
            }).collect::<StoreResult<Vec<_>>>()
        }).await.map_err(|e| StoreError::CorruptData(e.to_string()))??;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let assignment = load_assignment_tx(&mut tx, assignment_id).await?;
        match actor {
            TaskActor::Attempt(id) if id == expected_attempt_id => {}
            TaskActor::Attempt(_) => return Err(StoreError::RootAuthorityRequired),
            actor => actor.require_root_for(&assignment.root_session_id)?,
        }
        let current = load_current_attempt_tx(&mut tx, assignment_id).await?;
        if current.attempt_id != expected_attempt_id
            || current.state != AttemptState::Active
            || current.sealed_at.is_some()
        {
            return Err(StoreError::InvalidAssignment(
                "edit intents require the current active unsealed attempt".into(),
            ));
        }
        // Terminal/superseded attempts no longer own intents. Persisted history
        // remains in observations; deleting declarations grants nobody authority.
        sqlx::query("DELETE FROM edit_intents WHERE attempt_id IN (
            SELECT a.attempt_id FROM attempts a WHERE a.state <> ? OR a.sealed_at IS NOT NULL
            OR a.ordinal <> (SELECT MAX(b.ordinal) FROM attempts b WHERE b.assignment_id = a.assignment_id))")
            .bind(encode(&AttemptState::Active)?).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM edit_intents WHERE attempt_id = ?")
            .bind(expected_attempt_id.to_string())
            .execute(&mut *tx)
            .await?;
        let rows = sqlx::query("SELECT i.body_json, a.assignment_id, a.attempt_id, b.agent_path, b.thread_id
            FROM edit_intents i JOIN attempts a USING (attempt_id)
            JOIN assignment_repositories r USING (assignment_id)
            LEFT JOIN agent_task_bindings b ON b.assignment_id = a.assignment_id AND b.attempt_id = a.attempt_id
            WHERE r.workspace_id = ? ORDER BY a.assignment_id, i.ordinal")
            .bind(&context.assignment.workspace_id).fetch_all(&mut *tx).await?;
        let mut warnings = Vec::new();
        for row in rows {
            let competing: EditIntent = decode(&row.get::<String, _>("body_json"))?;
            for declared in &intents {
                // Canonical repository paths already resolve aliases; Windows
                // case folding conservatively includes case-sensitive folders.
                let same_path = if cfg!(windows) {
                    declared.path.eq_ignore_ascii_case(&competing.path)
                } else {
                    declared.path == competing.path
                };
                let same_base = declared.base_revision == competing.base_revision;
                let overlap = !same_base
                    || match (&declared.region, &competing.region) {
                        (Some(left), Some(right)) => {
                            left.start <= right.end && right.start <= left.end
                        }
                        _ => true,
                    };
                if same_path && overlap {
                    warnings.push(EditIntentWarning {
                        assignment_id: AssignmentId::parse(&row.get::<String, _>("assignment_id"))?,
                        attempt_id: AttemptId::parse(&row.get::<String, _>("attempt_id"))?,
                        agent_path: row.get("agent_path"),
                        thread_id: row.get("thread_id"),
                        declared: declared.clone(),
                        competing: competing.clone(),
                        same_base_revision: same_base,
                    });
                }
            }
        }
        for (ordinal, intent) in intents.iter().enumerate() {
            sqlx::query(
                "INSERT INTO edit_intents (attempt_id, ordinal, body_json) VALUES (?, ?, ?)",
            )
            .bind(expected_attempt_id.to_string())
            .bind(ordinal as i64)
            .bind(encode(intent)?)
            .execute(&mut *tx)
            .await?;
        }
        // Durable provenance reaches both tasks, including recipients currently
        // cold or owned by another process. The public handler additionally
        // queues advisory messages for reachable live peers.
        for warning in &warnings {
            let summary = format!(
                "Advisory edit-intent overlap: assignment {assignment_id}, attempt {expected_attempt_id} and assignment {}, attempt {}: {}. Declarations are not locks or overwrite permission. {}",
                warning.assignment_id,
                warning.attempt_id,
                warning.declared.path,
                encode(warning)?
            );
            append_observation_tx(
                &mut tx,
                &assignment,
                expected_attempt_id,
                ObservationKind::Editing,
                summary.clone(),
                None,
            )
            .await?;
            let competing = load_assignment_tx(&mut tx, warning.assignment_id).await?;
            append_observation_tx(
                &mut tx,
                &competing,
                warning.attempt_id,
                ObservationKind::Editing,
                summary,
                None,
            )
            .await?;
        }
        tx.commit().await?;
        self.notify_wake_waiters();
        Ok(warnings)
    }
}
