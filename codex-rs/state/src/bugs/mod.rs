//! Durable storage for user-supplied bug reports.
//!
//! Raw report text is bound directly into SQLite and is never logged here.

use crate::BUGS_DB_FILENAME;
use crate::migrations::runtime_bugs_migrator;
use anyhow::Context;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqlitePoolOptions;
use std::path::Path;
use std::time::Duration;

const MAX_ATTEMPTS: i64 = 3;
const STALE_CLAIM_SECONDS: i64 = 30 * 60;

#[derive(Clone, Debug)]
pub struct BugCreateParams<'a> {
    pub raw_text: &'a str,
    pub thread_id: &'a str,
    pub cwd: Option<&'a str>,
    pub repository_root: Option<&'a str>,
    pub git_commit: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BugCreateResult {
    pub id: i64,
    pub display_id: String,
}

#[derive(Clone, Debug)]
pub struct BugClaim {
    pub id: i64,
    pub raw_text: String,
    pub attempt_count: i64,
    pub claim_token: String,
}

#[derive(Clone, Copy, Debug)]
pub enum BugFailureCategory {
    Cancelled,
    Provider,
    MalformedOutput,
    Schema,
    Grounding,
}

impl BugFailureCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Provider => "provider",
            Self::MalformedOutput => "malformed_output",
            Self::Schema => "schema",
            Self::Grounding => "grounding",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BugClassification<'a> {
    pub summary: &'a str,
    pub severity: Option<&'a str>,
    pub failure_mechanism: Option<&'a str>,
    pub affected_components_json: &'a str,
    pub stated_cause: Option<&'a str>,
    pub required_repair: Option<&'a str>,
    pub classifier_provider_id: &'a str,
    pub classifier_requested_model: &'a str,
    pub classifier_resolved_model: Option<&'a str>,
    pub classifier_reasoning_effort: &'a str,
    pub classifier_schema_version: &'a str,
    pub classifier_prompt_version: &'a str,
}

#[derive(Clone)]
pub struct BugStore {
    pool: sqlx::SqlitePool,
}

impl BugStore {
    pub async fn open(home: &Path) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(home)
            .await
            .context("create bug database directory")?;
        let options = SqliteConnectOptions::new()
            .filename(home.join(BUGS_DB_FILENAME))
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .disable_statement_logging();
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        runtime_bugs_migrator()
            .run(&pool)
            .await
            .context("migrate bug database")?;
        Ok(Self { pool })
    }

    pub async fn create(&self, params: BugCreateParams<'_>) -> anyhow::Result<BugCreateResult> {
        let now = chrono::Utc::now().timestamp();
        let row = sqlx::query(
            "INSERT INTO bugs \
             (raw_text, created_at, updated_at, status, thread_id, cwd, repository_root, git_commit) \
             VALUES (?, ?, ?, 'pending', ?, ?, ?, ?) RETURNING id",
        )
        .bind(params.raw_text)
        .bind(now)
        .bind(now)
        .bind(params.thread_id)
        .bind(params.cwd)
        .bind(params.repository_root)
        .bind(params.git_commit)
        .fetch_one(&self.pool)
        .await?;
        let id = row.get("id");
        Ok(BugCreateResult {
            id,
            display_id: format_bug_id(id),
        })
    }

    pub async fn claim_by_id(&self, id: i64) -> anyhow::Result<Option<BugClaim>> {
        let now = chrono::Utc::now().timestamp();
        let token = uuid::Uuid::new_v4().to_string();
        let row = sqlx::query(
            "UPDATE bugs \
             SET attempt_count = attempt_count + 1, claim_token = ?, claim_timestamp = ?, updated_at = ? \
             WHERE id = ? AND status = 'pending' AND attempt_count < ? \
               AND (claim_token IS NULL OR claim_timestamp < ?) \
             RETURNING id, raw_text, attempt_count, claim_token",
        )
        .bind(&token)
        .bind(now)
        .bind(now)
        .bind(id)
        .bind(MAX_ATTEMPTS)
        .bind(now - STALE_CLAIM_SECONDS)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_claim))
    }

    pub async fn claim_next_older(&self, excluded_id: i64) -> anyhow::Result<Option<BugClaim>> {
        let now = chrono::Utc::now().timestamp();
        let token = uuid::Uuid::new_v4().to_string();
        let row = sqlx::query(
            "UPDATE bugs \
             SET attempt_count = attempt_count + 1, claim_token = ?, claim_timestamp = ?, updated_at = ? \
             WHERE id = (SELECT id FROM bugs \
                 WHERE id != ? AND status = 'pending' AND attempt_count < ? \
                   AND (claim_token IS NULL OR claim_timestamp < ?) \
                 ORDER BY created_at, id LIMIT 1) \
             AND status = 'pending' AND attempt_count < ? \
             AND (claim_token IS NULL OR claim_timestamp < ?) \
             RETURNING id, raw_text, attempt_count, claim_token",
        )
        .bind(&token)
        .bind(now)
        .bind(now)
        .bind(excluded_id)
        .bind(MAX_ATTEMPTS)
        .bind(now - STALE_CLAIM_SECONDS)
        .bind(MAX_ATTEMPTS)
        .bind(now - STALE_CLAIM_SECONDS)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_claim))
    }

    pub async fn commit_classification(
        &self,
        id: i64,
        claim_token: &str,
        value: BugClassification<'_>,
    ) -> anyhow::Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let result = sqlx::query(
            "UPDATE bugs SET status = 'classified', updated_at = ?, \
             claim_token = NULL, claim_timestamp = NULL, failure_category = NULL, \
             summary = ?, severity = ?, failure_mechanism = ?, affected_components = ?, \
             stated_cause = ?, required_repair = ?, classifier_provider_id = ?, \
             classifier_requested_model = ?, classifier_resolved_model = ?, \
             classifier_reasoning_effort = ?, classifier_schema_version = ?, \
             classifier_prompt_version = ?, classified_at = ? \
             WHERE id = ? AND status = 'pending' AND claim_token = ?",
        )
        .bind(now)
        .bind(value.summary)
        .bind(value.severity)
        .bind(value.failure_mechanism)
        .bind(value.affected_components_json)
        .bind(value.stated_cause)
        .bind(value.required_repair)
        .bind(value.classifier_provider_id)
        .bind(value.classifier_requested_model)
        .bind(value.classifier_resolved_model)
        .bind(value.classifier_reasoning_effort)
        .bind(value.classifier_schema_version)
        .bind(value.classifier_prompt_version)
        .bind(now)
        .bind(id)
        .bind(claim_token)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn release_failure(
        &self,
        id: i64,
        claim_token: &str,
        category: BugFailureCategory,
    ) -> anyhow::Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let result = sqlx::query(
            "UPDATE bugs SET claim_token = NULL, claim_timestamp = NULL, \
             failure_category = ?, updated_at = ? \
             WHERE id = ? AND status = 'pending' AND claim_token = ?",
        )
        .bind(category.as_str())
        .bind(now)
        .bind(id)
        .bind(claim_token)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }
}

fn row_to_claim(row: sqlx::sqlite::SqliteRow) -> BugClaim {
    BugClaim {
        id: row.get("id"),
        raw_text: row.get("raw_text"),
        attempt_count: row.get("attempt_count"),
        claim_token: row.get("claim_token"),
    }
}

pub fn format_bug_id(id: i64) -> String {
    format!("B{id:06}")
}

#[cfg(test)]
mod tests;
