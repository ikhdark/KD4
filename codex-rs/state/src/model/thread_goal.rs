use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
pub use codex_protocol::protocol::ThreadGoalStatus;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::epoch_millis_to_datetime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadGoal {
    pub thread_id: ThreadId,
    pub goal_id: String,
    pub objective: String,
    pub status: ThreadGoalStatus,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub(crate) struct ThreadGoalRow {
    pub thread_id: String,
    pub goal_id: String,
    pub objective: String,
    pub status: String,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl ThreadGoalRow {
    pub(crate) fn try_from_row(row: &SqliteRow) -> Result<Self> {
        Ok(Self {
            thread_id: row.try_get("thread_id")?,
            goal_id: row.try_get("goal_id")?,
            objective: row.try_get("objective")?,
            status: row.try_get("status")?,
            token_budget: row.try_get("token_budget")?,
            tokens_used: row.try_get("tokens_used")?,
            time_used_seconds: row.try_get("time_used_seconds")?,
            created_at_ms: row.try_get("created_at_ms")?,
            updated_at_ms: row.try_get("updated_at_ms")?,
        })
    }
}

impl TryFrom<ThreadGoalRow> for ThreadGoal {
    type Error = anyhow::Error;

    fn try_from(row: ThreadGoalRow) -> Result<Self> {
        Ok(Self {
            thread_id: ThreadId::try_from(row.thread_id)?,
            goal_id: row.goal_id,
            objective: row.objective,
            status: ThreadGoalStatus::try_from(row.status.as_str()).map_err(anyhow::Error::msg)?,
            token_budget: row.token_budget,
            tokens_used: row.tokens_used,
            time_used_seconds: row.time_used_seconds,
            created_at: epoch_millis_to_datetime(row.created_at_ms)?,
            updated_at: epoch_millis_to_datetime(row.updated_at_ms)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ThreadGoalStatus;

    #[test]
    fn goal_status_is_protocol_owned_with_stable_storage_labels() {
        let status: codex_protocol::protocol::ThreadGoalStatus = ThreadGoalStatus::UsageLimited;

        assert_eq!(status.as_str(), "usage_limited");
        assert_eq!(
            ThreadGoalStatus::try_from("complete"),
            Ok(ThreadGoalStatus::Complete)
        );
    }
}
