use super::*;
use crate::SortDirection;
use codex_protocol::protocol::SessionSource;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

/// Failure while inserting or replacing a persisted thread-spawn edge.
#[derive(Debug)]
pub enum ThreadSpawnEdgeWriteError {
    /// The requested edge would violate the persisted topology contract.
    InvalidRequest { message: String },
    /// SQLite failed while validating or writing the edge.
    Storage {
        operation: &'static str,
        source: sqlx::Error,
    },
}

impl std::fmt::Display for ThreadSpawnEdgeWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest { message } => f.write_str(message),
            Self::Storage { operation, source } => write!(f, "{operation}: {source}"),
        }
    }
}

impl std::error::Error for ThreadSpawnEdgeWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidRequest { .. } => None,
            Self::Storage { source, .. } => Some(source),
        }
    }
}

impl StateRuntime {
    /// Traverse current durable parents and live-only lineage in one read.
    pub async fn list_reconciled_thread_spawn_edges(
        &self,
        root: ThreadId,
        live_edges: &[(ThreadId, ThreadId)],
    ) -> anyhow::Result<Vec<(ThreadId, ThreadId)>> {
        let live_edges = live_edges
            .iter()
            .map(|(parent, child)| (child.to_string(), parent.to_string()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let rows = sqlx::query(
            r#"
WITH RECURSIVE live(parent, child) AS (
    SELECT value, key FROM json_each(?)
), reachable(id) AS (
    SELECT ?
    UNION
    SELECT edge.child_thread_id FROM thread_spawn_edges AS edge
    JOIN reachable ON edge.parent_thread_id = reachable.id
    UNION
    SELECT live.child FROM live JOIN reachable ON live.parent = reachable.id
    WHERE NOT EXISTS (SELECT 1 FROM thread_spawn_edges WHERE child_thread_id = live.child)
)
SELECT parent_thread_id AS parent, child_thread_id AS child
FROM thread_spawn_edges WHERE parent_thread_id IN (SELECT id FROM reachable)
UNION ALL
SELECT parent, child FROM live WHERE parent IN (SELECT id FROM reachable)
AND NOT EXISTS (SELECT 1 FROM thread_spawn_edges WHERE child_thread_id = live.child)
ORDER BY parent, child
            "#,
        )
        .bind(serde_json::to_string(&live_edges)?)
        .bind(root.to_string())
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    ThreadId::try_from(row.try_get::<String, _>("parent")?)?,
                    ThreadId::try_from(row.try_get::<String, _>("child")?)?,
                ))
            })
            .collect()
    }

    /// Fetches `(id, title, first_user_message)` for existing requested threads.
    /// A JSON array keeps this to one query without SQLite's bind-variable limit.
    pub async fn get_thread_titles(
        &self,
        ids: &[ThreadId],
    ) -> anyhow::Result<Vec<(ThreadId, String, Option<String>)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT id, title, first_user_message FROM threads \
             WHERE id IN (SELECT value FROM json_each(?))",
        )
        .bind(serde_json::to_string(ids)?)
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.into_iter()
            .map(|(id, title, first_user_message)| {
                Ok((ThreadId::try_from(id)?, title, first_user_message))
            })
            .collect()
    }

    pub async fn get_thread(&self, id: ThreadId) -> anyhow::Result<Option<crate::ThreadMetadata>> {
        let mut connection = self.pool.acquire().await?;
        Self::get_thread_on_connection(&mut connection, id).await
    }

    async fn get_thread_on_connection(
        connection: &mut sqlx::SqliteConnection,
        id: ThreadId,
    ) -> anyhow::Result<Option<crate::ThreadMetadata>> {
        let row = sqlx::query(
            r#"
SELECT
    threads.id,
    threads.session_id,
    (SELECT parent_thread_id FROM thread_spawn_edges WHERE child_thread_id = threads.id) AS current_parent_thread_id,
    threads.rollout_path,
    threads.created_at_ms AS created_at,
    threads.updated_at_ms AS updated_at,
    threads.recency_at_ms AS recency_at,
    threads.source,
    threads.history_mode,
    threads.thread_source,
    threads.agent_nickname,
    threads.agent_role,
    threads.agent_path,
    threads.model_provider,
    threads.model,
    threads.reasoning_effort,
    threads.cwd,
    threads.cli_version,
    threads.title,
    threads.preview,
    threads.sandbox_policy,
    threads.approval_mode,
    threads.tokens_used,
    threads.first_user_message,
    threads.archived_at,
    threads.project_id,
    threads.git_sha,
    threads.git_branch,
    threads.git_origin_url
FROM threads
WHERE threads.id = ?
            "#,
        )
        .bind(id.to_string())
        .fetch_optional(connection)
        .await?;
        row.map(|row| ThreadRow::try_from_row(&row).and_then(ThreadMetadata::try_from))
            .transpose()
    }

    pub async fn set_thread_preview_if_empty(
        &self,
        thread_id: ThreadId,
        preview: &str,
    ) -> anyhow::Result<bool> {
        let preview = preview.trim();
        if preview.is_empty() {
            return Ok(false);
        }
        let result = sqlx::query(
            r#"
UPDATE threads
SET preview = ?
WHERE id = ? AND preview = ''
            "#,
        )
        .bind(preview)
        .bind(thread_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Persist or replace the directional parent-child edge for a spawned thread.
    pub async fn upsert_thread_spawn_edge(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> Result<(), ThreadSpawnEdgeWriteError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|source| ThreadSpawnEdgeWriteError::Storage {
                operation: "begin thread-spawn edge write",
                source,
            })?;
        validate_thread_spawn_edge(&mut tx, parent_thread_id, child_thread_id).await?;
        sqlx::query(
            r#"
INSERT INTO thread_spawn_edges (
    parent_thread_id,
    child_thread_id,
    status
) VALUES (?, ?, ?)
ON CONFLICT(child_thread_id) DO UPDATE SET
    parent_thread_id = excluded.parent_thread_id,
    status = excluded.status
            "#,
        )
        .bind(parent_thread_id.to_string())
        .bind(child_thread_id.to_string())
        .bind(status.as_ref())
        .execute(&mut *tx)
        .await
        .map_err(|source| ThreadSpawnEdgeWriteError::Storage {
            operation: "upsert thread-spawn edge",
            source,
        })?;
        tx.commit()
            .await
            .map_err(|source| ThreadSpawnEdgeWriteError::Storage {
                operation: "commit thread-spawn edge write",
                source,
            })?;
        Ok(())
    }

    /// Update the persisted lifecycle status of a spawned thread's incoming edge.
    pub async fn set_thread_spawn_edge_status(
        &self,
        child_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE thread_spawn_edges SET status = ? WHERE child_thread_id = ?")
            .bind(status.as_ref())
            .bind(child_thread_id.to_string())
            .execute(self.pool.as_ref())
            .await?;
        Ok(())
    }

    /// List direct spawned children of `parent_thread_id` whose edge matches `status`.
    pub async fn list_thread_spawn_children_with_status(
        &self,
        parent_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_children_matching(parent_thread_id, Some(status))
            .await
    }

    /// List all direct spawned children of `parent_thread_id`.
    pub async fn list_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_children_matching(parent_thread_id, /*status*/ None)
            .await
    }

    /// List spawned descendants of `root_thread_id` whose edges match `status`.
    ///
    /// Descendants are returned breadth-first by depth, then by thread id for stable ordering.
    pub async fn list_thread_spawn_descendants_with_status(
        &self,
        root_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_descendants_matching(root_thread_id, Some(status))
            .await
    }

    /// List all spawned descendants of `root_thread_id`.
    ///
    /// Descendants are returned breadth-first by depth, then by thread id for stable ordering.
    pub async fn list_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_descendants_matching(root_thread_id, /*status*/ None)
            .await
    }

    async fn list_thread_spawn_children_matching(
        &self,
        parent_thread_id: ThreadId,
        status: Option<crate::DirectionalThreadSpawnEdgeStatus>,
    ) -> anyhow::Result<Vec<ThreadId>> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT child_thread_id FROM thread_spawn_edges WHERE parent_thread_id = ",
        );
        builder.push_bind(parent_thread_id.to_string());
        if let Some(status) = status {
            builder.push(" AND status = ").push_bind(status.to_string());
        }
        builder.push(" ORDER BY child_thread_id");

        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        rows.into_iter()
            .map(|row| {
                ThreadId::try_from(row.try_get::<String, _>("child_thread_id")?).map_err(Into::into)
            })
            .collect()
    }

    async fn list_thread_spawn_descendants_matching(
        &self,
        root_thread_id: ThreadId,
        status: Option<crate::DirectionalThreadSpawnEdgeStatus>,
    ) -> anyhow::Result<Vec<ThreadId>> {
        // The unique incoming-child key means a reachable cycle must include the root.
        // Excluding it terminates legacy cycles without quadratic ancestor-string work.
        let root_thread_id = root_thread_id.to_string();
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
WITH RECURSIVE subtree(child_thread_id, depth) AS (
    SELECT child_thread_id, 1
    FROM thread_spawn_edges
    WHERE parent_thread_id =
            "#,
        );
        builder.push_bind(root_thread_id.clone());
        builder.push(" AND child_thread_id != ");
        builder.push_bind(root_thread_id.clone());
        if let Some(status) = status {
            let status = status.to_string();
            builder.push(" AND status = ").push_bind(status.clone());
            builder.push(
                r#"
    UNION ALL
    SELECT edge.child_thread_id, subtree.depth + 1
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
    WHERE edge.child_thread_id !=
                "#,
            );
            builder.push_bind(root_thread_id);
            builder.push(" AND edge.status = ");
            builder.push_bind(status);
        } else {
            builder.push(
                r#"
    UNION ALL
    SELECT edge.child_thread_id, subtree.depth + 1
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
    WHERE edge.child_thread_id !=
                "#,
            );
            builder.push_bind(root_thread_id);
        }
        builder.push(
            r#"
)
SELECT child_thread_id
FROM subtree
ORDER BY depth ASC, child_thread_id ASC
            "#,
        );

        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        rows.into_iter()
            .map(|row| {
                ThreadId::try_from(row.try_get::<String, _>("child_thread_id")?).map_err(Into::into)
            })
            .collect()
    }

    /// Persist an open parent-child edge unless the child already has an incoming edge.
    pub async fn insert_thread_spawn_edge_if_absent(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
    ) -> Result<(), ThreadSpawnEdgeWriteError> {
        // Rollout updates repeatedly infer the same edge. A no-op must not
        // reserve SQLite's sole writer; WAL readers can check it concurrently.
        // A miss is only a hint: recheck under BEGIN IMMEDIATE below before
        // validating topology or inserting, since another writer may win.
        let edge_exists = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM thread_spawn_edges WHERE child_thread_id = ?)",
        )
        .bind(child_thread_id.to_string())
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|source| ThreadSpawnEdgeWriteError::Storage {
            operation: "check inferred thread-spawn edge",
            source,
        })? != 0;
        if edge_exists {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|source| ThreadSpawnEdgeWriteError::Storage {
                operation: "begin inferred thread-spawn edge write",
                source,
            })?;
        let edge_exists = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM thread_spawn_edges WHERE child_thread_id = ?)",
        )
        .bind(child_thread_id.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(|source| ThreadSpawnEdgeWriteError::Storage {
            operation: "check inferred thread-spawn edge",
            source,
        })? != 0;
        if edge_exists {
            tx.commit()
                .await
                .map_err(|source| ThreadSpawnEdgeWriteError::Storage {
                    operation: "commit inferred thread-spawn edge check",
                    source,
                })?;
            return Ok(());
        }
        validate_thread_spawn_edge(&mut tx, parent_thread_id, child_thread_id).await?;
        sqlx::query(
            r#"
INSERT INTO thread_spawn_edges (
    parent_thread_id,
    child_thread_id,
    status
) VALUES (?, ?, ?)
ON CONFLICT(child_thread_id) DO NOTHING
            "#,
        )
        .bind(parent_thread_id.to_string())
        .bind(child_thread_id.to_string())
        .bind(crate::DirectionalThreadSpawnEdgeStatus::Open.as_ref())
        .execute(&mut *tx)
        .await
        .map_err(|source| ThreadSpawnEdgeWriteError::Storage {
            operation: "insert inferred thread-spawn edge",
            source,
        })?;
        tx.commit()
            .await
            .map_err(|source| ThreadSpawnEdgeWriteError::Storage {
                operation: "commit inferred thread-spawn edge write",
                source,
            })?;
        Ok(())
    }

    async fn insert_thread_spawn_edge_from_source_if_absent(
        &self,
        child_thread_id: ThreadId,
        source: &str,
    ) -> anyhow::Result<()> {
        let Some(parent_thread_id) = thread_spawn_parent_thread_id_from_source_str(source) else {
            return Ok(());
        };
        self.infer_thread_spawn_edge_if_absent(parent_thread_id, child_thread_id)
            .await
    }

    /// Persist lineage inferred from history, typically after a metadata write.
    ///
    /// An explicitly deleted endpoint means the edge is intentionally absent: the
    /// deletion trigger still rejects the insert, but that settles inference instead
    /// of failing a surviving thread's already-committed metadata. Explicit edge
    /// writes keep the rejection through [`Self::insert_thread_spawn_edge_if_absent`].
    pub async fn infer_thread_spawn_edge_if_absent(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
    ) -> anyhow::Result<()> {
        let Err(err) = self
            .insert_thread_spawn_edge_if_absent(parent_thread_id, child_thread_id)
            .await
        else {
            return Ok(());
        };
        let deleted_owner = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM deleted_thread_owners WHERE thread_id IN (?, ?))",
        )
        .bind(parent_thread_id.to_string())
        .bind(child_thread_id.to_string())
        .fetch_one(self.pool.as_ref())
        .await?;
        if deleted_owner != 0 {
            return Ok(());
        }
        Err(err.into())
    }

    /// Find a rollout path by thread id using the underlying database.
    pub async fn find_rollout_path_by_id(
        &self,
        id: ThreadId,
        archived_only: Option<bool>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let mut builder =
            QueryBuilder::<Sqlite>::new("SELECT rollout_path FROM threads WHERE id = ");
        builder.push_bind(id.to_string());
        match archived_only {
            Some(true) => {
                builder.push(" AND archived = 1");
            }
            Some(false) => {
                builder.push(" AND archived = 0");
            }
            None => {}
        }
        let row = builder.build().fetch_optional(self.pool.as_ref()).await?;
        Ok(row
            .map(|r| r.try_get::<String, _>("rollout_path"))
            .transpose()?
            .map(PathBuf::from))
    }

    /// Find threads whose user-facing title exactly matches `title`, newest first.
    ///
    /// There is no cwd filter: metadata writers store a canonicalized cwd, which need not match
    /// a caller's configured cwd textually, so callers compare cwd after path normalization.
    pub async fn find_threads_by_exact_title(
        &self,
        title: &str,
        allowed_sources: &[String],
        model_providers: Option<&[String]>,
        archived_only: bool,
    ) -> anyhow::Result<Vec<crate::ThreadMetadata>> {
        let mut builder = QueryBuilder::<Sqlite>::new("");
        push_thread_select_columns(&mut builder);
        builder.push(" FROM threads");
        push_thread_filters(
            &mut builder,
            ThreadFilterOptions {
                project_id: None,
                archived_only,
                allowed_sources,
                model_providers,
                cwd_filters: None,
                anchor: None,
                sort_key: crate::SortKey::UpdatedAt,
                sort_direction: SortDirection::Desc,
                search_term: None,
            },
            /*include_empty_preview*/ false,
            /*include_thread_id_tiebreaker*/ false,
        );
        builder.push(" AND threads.title = ");
        builder.push_bind(title);
        builder.push(" ORDER BY threads.updated_at_ms DESC, threads.id DESC");

        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        rows.iter()
            .map(|row| ThreadRow::try_from_row(row).and_then(crate::ThreadMetadata::try_from))
            .collect()
    }

    /// List threads using the underlying database.
    pub async fn list_threads(
        &self,
        page_size: usize,
        filters: ThreadFilterOptions<'_>,
    ) -> anyhow::Result<crate::ThreadsPage> {
        self.list_threads_matching(page_size, filters, /*relation_filter*/ None)
            .await
    }

    /// List threads matching a persisted spawn-graph relationship.
    pub async fn list_threads_by_relation(
        &self,
        page_size: usize,
        relation_filter: crate::ThreadRelationFilter,
        filters: ThreadFilterOptions<'_>,
    ) -> anyhow::Result<crate::ThreadsPage> {
        self.list_threads_matching(page_size, filters, Some(relation_filter))
            .await
    }

    async fn list_threads_matching(
        &self,
        page_size: usize,
        filters: ThreadFilterOptions<'_>,
        relation_filter: Option<crate::ThreadRelationFilter>,
    ) -> anyhow::Result<crate::ThreadsPage> {
        let limit = page_size.saturating_add(1);

        let mut builder = QueryBuilder::<Sqlite>::new("");
        push_list_threads_query(&mut builder, filters, relation_filter, limit);

        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        let mut items = Vec::with_capacity(rows.len());
        let mut parent_thread_ids = std::collections::HashMap::new();
        for row in rows {
            let item = ThreadRow::try_from_row(&row).and_then(ThreadMetadata::try_from)?;
            if let Some(parent_thread_id) = row.try_get::<Option<String>, _>("parent_thread_id")? {
                parent_thread_ids.insert(item.id, ThreadId::try_from(parent_thread_id)?);
            }
            items.push(item);
        }
        let num_scanned_rows = items.len();
        let next_anchor = if items.len() > page_size {
            if let Some(overflow_item) = items.pop() {
                parent_thread_ids.remove(&overflow_item.id);
            }
            items
                .last()
                .and_then(|item| anchor_from_item(item, filters.sort_key))
        } else {
            None
        };
        Ok(ThreadsPage {
            items,
            parent_thread_ids,
            next_anchor,
            num_scanned_rows,
        })
    }

    /// List thread ids using the underlying database (no rollout scanning).
    pub async fn list_thread_ids(
        &self,
        limit: usize,
        anchor: Option<&crate::Anchor>,
        sort_key: crate::SortKey,
        allowed_sources: &[String],
        model_providers: Option<&[String]>,
        archived_only: bool,
    ) -> anyhow::Result<Vec<ThreadId>> {
        let mut builder = QueryBuilder::<Sqlite>::new("SELECT threads.id FROM threads");
        push_thread_filters(
            &mut builder,
            ThreadFilterOptions {
                project_id: None,
                archived_only,
                allowed_sources,
                model_providers,
                cwd_filters: None,
                anchor,
                sort_key,
                sort_direction: SortDirection::Desc,
                search_term: None,
            },
            /*include_empty_preview*/ false,
            /*include_thread_id_tiebreaker*/ true,
        );
        push_thread_order_and_limit(
            &mut builder,
            sort_key,
            SortDirection::Desc,
            OrderByIndex::Enabled,
            /*include_thread_id_tiebreaker*/ true,
            limit,
        );

        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        rows.into_iter()
            .map(|row| {
                let id: String = row.try_get("id")?;
                Ok(ThreadId::try_from(id)?)
            })
            .collect()
    }

    /// Insert or replace thread metadata directly.
    pub async fn upsert_thread(&self, metadata: &crate::ThreadMetadata) -> anyhow::Result<()> {
        self.upsert_thread_with_timestamps(metadata, /*allocate_timestamps*/ true)
            .await
    }

    /// Apply a metadata patch to the current row under the SQLite writer
    /// reservation. The fallback is used only if no row exists at admission.
    pub async fn patch_thread_metadata(
        &self,
        fallback: &crate::ThreadMetadata,
        patch: impl FnOnce(&mut crate::ThreadMetadata) -> Result<(), String> + Send,
    ) -> anyhow::Result<crate::ThreadMetadata> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut metadata = Self::get_thread_on_connection(&mut tx, fallback.id)
            .await?
            .unwrap_or_else(|| fallback.clone());
        patch(&mut metadata).map_err(anyhow::Error::msg)?;
        if let Some(id) = metadata.project_id.as_deref() {
            let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE id = ?")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
            anyhow::ensure!(exists != 0, "project not found: {id}");
        }
        self.upsert_thread_on_connection(&mut tx, &metadata, true)
            .await?;
        // Explicit patches can clear Git/project fields; rollout upserts instead
        // deliberately preserve those fields. Both writes share this transaction.
        sqlx::query("UPDATE threads SET git_sha = ?, git_branch = ?, git_origin_url = ?, project_id = ? WHERE id = ?")
            .bind(metadata.git_sha.as_deref())
            .bind(metadata.git_branch.as_deref())
            .bind(metadata.git_origin_url.as_deref())
            .bind(metadata.project_id.as_deref())
            .bind(metadata.id.to_string())
            .execute(&mut *tx).await?;
        let metadata = Self::get_thread_on_connection(&mut tx, metadata.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("thread metadata disappeared during patch"))?;
        tx.commit().await?;
        self.insert_thread_spawn_edge_from_source_if_absent(metadata.id, metadata.source.as_str())
            .await?;
        Ok(metadata)
    }

    /// Insert rollout metadata without changing its persisted file timestamps.
    pub async fn upsert_thread_preserving_timestamps(
        &self,
        metadata: &crate::ThreadMetadata,
    ) -> anyhow::Result<()> {
        self.upsert_thread_with_timestamps(metadata, /*allocate_timestamps*/ false)
            .await
    }

    /// Read repair owns only its named fields. Do not recreate a concurrently
    /// deleted row or allocate artificial recency for an otherwise read-only operation.
    pub async fn patch_existing_thread_metadata(
        &self,
        id: ThreadId,
        patch: impl FnOnce(&mut crate::ThreadMetadata) + Send,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(mut current) = Self::get_thread_on_connection(&mut tx, id).await? else {
            return Ok(false);
        };
        let previous = current.clone();
        patch(&mut current);
        if current != previous {
            self.upsert_thread_on_connection(&mut tx, &current, false)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Merge filesystem extraction against current authority under the write
    /// reservation. An older snapshot cannot replace a newer explicit update.
    pub async fn thread_metadata_for_rollout_path(
        &self,
        path: &Path,
    ) -> anyhow::Result<Option<crate::ThreadMetadata>> {
        let id: Option<String> =
            sqlx::query_scalar("SELECT id FROM threads WHERE rollout_path IN (?, ?) LIMIT 1")
                .bind(path.display().to_string())
                .bind(path.with_extension("jsonl.zst").display().to_string())
                .fetch_optional(self.pool.as_ref())
                .await?;
        match id {
            Some(id) => self.get_thread(ThreadId::from_string(&id)?).await,
            None => Ok(None),
        }
    }

    pub async fn reconcile_thread_metadata(
        &self,
        extracted: &crate::ThreadMetadata,
        expected: Option<&crate::ThreadMetadata>,
        source_version: &(PathBuf, u64, Option<std::time::SystemTime>),
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let source = tokio::fs::metadata(&source_version.0).await?;
        anyhow::ensure!(
            source.len() == source_version.1 && source.modified().ok() == source_version.2,
            "rollout changed during metadata extraction; reconciliation remains pending"
        );
        let current = Self::get_thread_on_connection(&mut tx, extracted.id).await?;
        if expected.is_none()
            && let Some(mut current) = current.clone()
            && current.rollout_path != extracted.rollout_path
        {
            // A completed move can leave the index under its previous path.
            // Repair only location now, then retry extraction from this base.
            current.rollout_path = extracted.rollout_path.clone();
            current.archived_at = extracted.archived_at;
            self.upsert_thread_on_connection(&mut tx, &current, false)
                .await?;
            tx.commit().await?;
            anyhow::bail!("rollout location repaired; retry metadata reconciliation");
        }
        anyhow::ensure!(
            current.as_ref() == expected,
            "thread metadata changed during extraction; stale snapshot rejected"
        );
        let mut merged = extracted.clone();
        if let Some(current) = current {
            if extracted.updated_at < current.updated_at {
                // Identity is immutable and can be recovered independently of newer
                // mutable settings. Do not leave legacy rows permanently unknown.
                if current.session_id.is_none()
                    && let Some(session_id) = extracted.session_id
                {
                    sqlx::query(
                        "UPDATE threads SET session_id = ? WHERE id = ? AND session_id IS NULL",
                    )
                    .bind(session_id.to_string())
                    .bind(current.id.to_string())
                    .execute(&mut *tx)
                    .await?;
                }
                // Path/archive repair is a separate, narrow operation. The
                // source does not authorize reverting any current row fields.
                tx.commit().await?;
                return Ok(());
            }
            merged.prefer_existing_git_info(&current);
            merged.prefer_existing_explicit_title(&current);
            // A legacy rollout with no model observation cannot clear the last
            // observed model. A known canonical model still takes precedence.
            merged.model = merged.model.or(current.model);
        }
        self.upsert_thread_on_connection(&mut tx, &merged, false)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn update_thread_title(
        &self,
        thread_id: ThreadId,
        title: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE threads SET title = ? WHERE id = ?")
            .bind(title)
            .bind(thread_id.to_string())
            .execute(self.pool.as_ref())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn touch_thread_updated_at(
        &self,
        thread_id: ThreadId,
        updated_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let updated_at = self.allocate_thread_updated_at(updated_at)?;
        let result =
            sqlx::query("UPDATE threads SET updated_at = (MAX(?1, updated_at_ms + 1) / 1000 - (MAX(?1, updated_at_ms + 1) % 1000 < 0)), updated_at_ms = MAX(?, updated_at_ms + 1) WHERE id = ?")
                .bind(datetime_to_epoch_millis(updated_at))
                .bind(datetime_to_epoch_millis(updated_at))
                .bind(thread_id.to_string())
                .execute(self.pool.as_ref())
                .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn touch_thread_recency_at(
        &self,
        thread_id: ThreadId,
        recency_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let recency_at = self.allocate_thread_recency_at(recency_at)?;
        let recency_at_seconds = datetime_to_epoch_seconds(recency_at);
        let recency_at_millis = datetime_to_epoch_millis(recency_at);
        let result = sqlx::query(
            r#"
UPDATE threads
SET
    recency_at = MAX(?, MAX(?, recency_at_ms + 1) / 1000),
    recency_at_ms = MAX(?, recency_at_ms + 1)
WHERE id = ?
            "#,
        )
        .bind(recency_at_seconds)
        .bind(recency_at_millis)
        .bind(recency_at_millis)
        .bind(thread_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Allocate a persisted `updated_at` value for thread-list cursor ordering.
    ///
    /// Live writes get unique monotonic milliseconds. Backfill/repair callers
    /// explicitly use the timestamp-preserving APIs instead of a distance heuristic.
    fn allocate_thread_updated_at(
        &self,
        updated_at: DateTime<Utc>,
    ) -> anyhow::Result<DateTime<Utc>> {
        allocate_thread_timestamp(self.thread_updated_at_millis.as_ref(), updated_at)
    }

    fn allocate_thread_recency_at(
        &self,
        recency_at: DateTime<Utc>,
    ) -> anyhow::Result<DateTime<Utc>> {
        allocate_thread_timestamp(self.thread_recency_at_millis.as_ref(), recency_at)
    }
}

fn allocate_thread_timestamp(
    high_water_mark: &AtomicI64,
    timestamp: DateTime<Utc>,
) -> anyhow::Result<DateTime<Utc>> {
    let candidate = datetime_to_epoch_millis(timestamp);
    let allocated = loop {
        let current = high_water_mark.load(Ordering::Relaxed);

        // New wall-clock time: advance the process-local high-water mark and use it as-is.
        if candidate > current {
            if high_water_mark
                .compare_exchange(current, candidate, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break candidate;
            }
            continue;
        }

        let bumped = current.checked_add(1).ok_or_else(|| anyhow::anyhow!("thread timestamp overflow"))?;
        if high_water_mark
            .compare_exchange(current, bumped, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break bumped;
        }
    };
    epoch_millis_to_datetime(allocated)
}

impl StateRuntime {
    async fn upsert_thread_with_timestamps(
        &self,
        metadata: &crate::ThreadMetadata,
        allocate_timestamps: bool,
    ) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire().await?;
        self.upsert_thread_on_connection(&mut connection, metadata, allocate_timestamps)
            .await?;
        drop(connection);
        self.insert_thread_spawn_edge_from_source_if_absent(metadata.id, metadata.source.as_str())
            .await?;
        Ok(())
    }

    async fn upsert_thread_on_connection(
        &self,
        connection: &mut sqlx::SqliteConnection,
        metadata: &crate::ThreadMetadata,
        allocate_timestamps: bool,
    ) -> anyhow::Result<()> {
        let updated_at = if allocate_timestamps {
            self.allocate_thread_updated_at(metadata.updated_at)?
        } else {
            metadata.updated_at
        };
        let insert_recency_at = if allocate_timestamps {
            self.allocate_thread_recency_at(metadata.recency_at)?
        } else {
            metadata.recency_at
        };
        let preview = metadata_preview(metadata);
        // Backfill/reconcile callers merge existing git info before upserting, but that
        // read/modify/write is not atomic. Preserve non-null SQLite git fields here so
        // an explicit metadata update cannot be lost if a stale rollout upsert lands later.
        sqlx::query(
            r#"
INSERT INTO threads (
    id,
    session_id,
    rollout_path,
    created_at,
    updated_at,
    recency_at,
    created_at_ms,
    updated_at_ms,
    recency_at_ms,
    source,
    history_mode,
    thread_source,
    agent_nickname,
    agent_role,
    agent_path,
    model_provider,
    model,
    reasoning_effort,
    cwd,
    cli_version,
    title,
    preview,
    sandbox_policy,
    approval_mode,
    tokens_used,
    first_user_message,
    archived,
    archived_at,
    git_sha,
    git_branch,
    git_origin_url,
    project_id
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO UPDATE SET
    session_id = COALESCE(threads.session_id, excluded.session_id),
    rollout_path = excluded.rollout_path,
    created_at = excluded.created_at,
    updated_at = CASE WHEN ? THEN (MAX(excluded.updated_at_ms, threads.updated_at_ms + 1) / 1000 - (MAX(excluded.updated_at_ms, threads.updated_at_ms + 1) % 1000 < 0)) ELSE excluded.updated_at END,
    recency_at = threads.recency_at,
    created_at_ms = excluded.created_at_ms,
    updated_at_ms = CASE WHEN ? THEN MAX(excluded.updated_at_ms, threads.updated_at_ms + 1) ELSE excluded.updated_at_ms END,
    recency_at_ms = threads.recency_at_ms,
    source = excluded.source,
    history_mode = excluded.history_mode,
    thread_source = excluded.thread_source,
    agent_nickname = excluded.agent_nickname,
    agent_role = excluded.agent_role,
    agent_path = excluded.agent_path,
    model_provider = excluded.model_provider,
    model = excluded.model,
    reasoning_effort = excluded.reasoning_effort,
    cwd = excluded.cwd,
    cli_version = excluded.cli_version,
    title = excluded.title,
    preview = COALESCE(NULLIF(excluded.preview, ''), threads.preview),
    sandbox_policy = excluded.sandbox_policy,
    approval_mode = excluded.approval_mode,
    tokens_used = excluded.tokens_used,
    first_user_message = excluded.first_user_message,
    archived = excluded.archived,
    archived_at = excluded.archived_at,
    git_sha = COALESCE(threads.git_sha, excluded.git_sha),
    git_branch = COALESCE(threads.git_branch, excluded.git_branch),
    git_origin_url = COALESCE(threads.git_origin_url, excluded.git_origin_url)
            "#,
        )
        .bind(metadata.id.to_string())
        .bind(metadata.session_id.map(|id| id.to_string()))
        .bind(metadata.rollout_path.display().to_string())
        .bind(datetime_to_epoch_seconds(metadata.created_at))
        .bind(datetime_to_epoch_seconds(updated_at))
        .bind(datetime_to_epoch_seconds(insert_recency_at))
        .bind(datetime_to_epoch_millis(metadata.created_at))
        .bind(datetime_to_epoch_millis(updated_at))
        .bind(datetime_to_epoch_millis(insert_recency_at))
        .bind(metadata.source.as_str())
        .bind(metadata.history_mode.as_str())
        .bind(
            metadata
                .thread_source
                .as_ref()
                .map(codex_protocol::protocol::ThreadSource::as_str),
        )
        .bind(metadata.agent_nickname.as_deref())
        .bind(metadata.agent_role.as_deref())
        .bind(metadata.agent_path.as_deref())
        .bind(metadata.model_provider.as_str())
        .bind(metadata.model.as_deref())
        .bind(
            metadata
                .reasoning_effort
                .as_ref()
                .map(crate::extract::enum_to_string),
        )
        .bind(metadata.cwd.display().to_string())
        .bind(metadata.cli_version.as_str())
        .bind(metadata.title.as_str())
        .bind(preview)
        .bind(metadata.sandbox_policy.as_str())
        .bind(metadata.approval_mode.as_str())
        .bind(metadata.tokens_used)
        .bind(metadata.first_user_message.as_deref().unwrap_or_default())
        .bind(metadata.archived_at.is_some())
        .bind(metadata.archived_at.map(datetime_to_epoch_seconds))
        .bind(metadata.git_sha.as_deref())
        .bind(metadata.git_branch.as_deref())
        .bind(metadata.git_origin_url.as_deref())
        .bind(metadata.project_id.as_deref())
        .bind(allocate_timestamps)
        .bind(allocate_timestamps)
        .execute(connection)
        .await?;
        Ok(())
    }

    /// Apply rollout items incrementally using the underlying database.
    pub async fn apply_rollout_items(
        &self,
        builder: &ThreadMetadataBuilder,
        items: &[RolloutItem],
        updated_at_override: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        // Resolve filesystem data before taking the SQLite write reservation.
        let updated_at = match updated_at_override {
            Some(updated_at) => Some(updated_at),
            None => file_modified_time_utc(builder.rollout_path.as_path()).await,
        };
        // Reserve the writer before reading: a deferred transaction could read a
        // stale snapshot and then fail to upgrade after another writer commits.
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing_metadata =
            Self::get_thread_on_connection(&mut transaction, builder.id).await?;
        let mut metadata = existing_metadata
            .clone()
            .unwrap_or_else(|| builder.build(&self.default_provider));
        metadata.session_id = metadata.session_id.or(builder.session_id);
        metadata.rollout_path = builder.rollout_path.clone();
        crate::extract::apply_rollout_items(&mut metadata, items, &self.default_provider);
        if let Some(existing_metadata) = existing_metadata.as_ref() {
            metadata.prefer_existing_git_info(existing_metadata);
        }
        if let Some(updated_at) = updated_at {
            metadata.updated_at = updated_at;
        }
        self.upsert_thread_on_connection(&mut transaction, &metadata, true)
            .await?;
        transaction.commit().await?;
        self.insert_thread_spawn_edge_from_source_if_absent(metadata.id, metadata.source.as_str())
            .await?;
        if let Some(parent_thread_id) = builder.parent_thread_id {
            self.infer_thread_spawn_edge_if_absent(parent_thread_id, builder.id)
                .await?;
        }
        Ok(())
    }

    /// Mark a thread as archived using the underlying database.
    pub async fn mark_archived(
        &self,
        thread_id: ThreadId,
        rollout_path: &Path,
        archived_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.update_thread_archive_state(thread_id, rollout_path, Some(archived_at))
            .await
    }

    /// Mark a thread as unarchived using the underlying database.
    pub async fn mark_unarchived(
        &self,
        thread_id: ThreadId,
        rollout_path: &Path,
    ) -> anyhow::Result<()> {
        self.update_thread_archive_state(thread_id, rollout_path, None)
            .await
    }

    async fn update_thread_archive_state(
        &self,
        thread_id: ThreadId,
        rollout_path: &Path,
        archived_at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        let updated_at = file_modified_time_utc(rollout_path)
            .await
            .map(|time| self.allocate_thread_updated_at(time))
            .transpose()?;
        // Archive state does not own titles, usage, models, or other metadata.
        sqlx::query("UPDATE threads SET rollout_path = ?, archived = ?, archived_at = ?, updated_at = CASE WHEN ?4 IS NULL THEN updated_at ELSE (MAX(?4, updated_at_ms + 1) / 1000 - (MAX(?4, updated_at_ms + 1) % 1000 < 0)) END, updated_at_ms = CASE WHEN ?4 IS NULL THEN updated_at_ms ELSE MAX(?4, updated_at_ms + 1) END WHERE id = ?5")
            .bind(rollout_path.display().to_string())
            .bind(archived_at.is_some())
            .bind(archived_at.map(datetime_to_epoch_seconds))
            .bind(updated_at.map(datetime_to_epoch_millis))
            .bind(thread_id.to_string())
            .execute(self.pool.as_ref()).await?;
        Ok(())
    }

    /// Delete a thread and all associated state by id.
    pub async fn delete_thread(&self, thread_id: ThreadId) -> anyhow::Result<u64> {
        self.delete_threads_strict(&[thread_id]).await
    }

    /// Delete a set of threads and all associated state.
    ///
    /// The primary state transaction is the commit point. Auxiliary databases are cleaned only
    /// after that transaction commits, so a primary failure leaves the complete retry graph and
    /// auxiliary records intact. Auxiliary cleanup is best-effort after the logical delete.
    pub async fn delete_threads_strict(&self, thread_ids: &[ThreadId]) -> anyhow::Result<u64> {
        let (rows_affected, _) = self.delete_threads_strict_impl(thread_ids).await?;
        Ok(rows_affected)
    }

    async fn delete_threads_strict_impl(
        &self,
        thread_ids: &[ThreadId],
    ) -> anyhow::Result<(u64, usize)> {
        self.delete_threads_with_boundary(thread_ids, None).await
    }

    /// Validate the selected subtree under the same writer reservation as deletion.
    /// A crossing edge requires replanning while staged files can still roll back.
    pub async fn delete_thread_subtree_strict(
        &self,
        root: ThreadId,
        thread_ids: &[ThreadId],
    ) -> anyhow::Result<u64> {
        let (rows, _) = self
            .delete_threads_with_boundary(thread_ids, Some(root))
            .await?;
        Ok(rows)
    }

    /// Explicit restore/import authorization only; ordinary metadata inference
    /// must never clear deletion evidence. Receipts otherwise have no expiry.
    /// Replace any ThreadManager that retired these UUIDs before reloading them;
    /// its in-process retirement fence deliberately lasts for that manager's lifetime.
    pub async fn restore_deleted_thread_owners(
        &self,
        thread_ids: &[ThreadId],
    ) -> anyhow::Result<()> {
        let ids = thread_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        sqlx::query(
            "DELETE FROM deleted_thread_owners WHERE thread_id IN (SELECT value FROM json_each(?))",
        )
        .bind(serde_json::to_string(&ids)?)
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    async fn delete_threads_with_boundary(
        &self,
        thread_ids: &[ThreadId],
        root: Option<ThreadId>,
    ) -> anyhow::Result<(u64, usize)> {
        if thread_ids.is_empty() {
            return Ok((0, 0));
        }

        let thread_id_strings = thread_ids
            .iter()
            .map(ThreadId::to_string)
            .collect::<Vec<_>>();
        let thread_ids_json = serde_json::to_string(&thread_id_strings)?;
        let now = Utc::now().timestamp();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut primary_statement_count = 0;
        if let Some(root) = root {
            anyhow::ensure!(
                thread_ids.contains(&root),
                "subtree deletion must include its root"
            );
            let crossing: i64 = sqlx::query_scalar(
                r#"
WITH selected(id) AS (SELECT value FROM json_each(?))
SELECT EXISTS (
    SELECT 1 FROM thread_spawn_edges
    WHERE (parent_thread_id IN selected AND child_thread_id NOT IN selected)
       OR (child_thread_id IN selected AND child_thread_id != ? AND parent_thread_id NOT IN selected)
)
                "#,
            ).bind(&thread_ids_json).bind(root.to_string()).fetch_one(&mut *tx).await?;
            anyhow::ensure!(
                crossing == 0,
                "subtree topology changed during deletion; retry the operation"
            );
        }

        // If both the job runner and worker are being deleted, requeueing the worker item would
        // leave a running job with no loop to consume it.
        sqlx::query(
            r#"
UPDATE agent_jobs
SET status = ?, updated_at = ?, completed_at = ?, last_error = ?
WHERE status IN (?, ?)
  AND id IN (
    SELECT item.job_id
    FROM agent_job_items AS item
    JOIN thread_spawn_edges AS edge ON edge.child_thread_id = item.assigned_thread_id
    WHERE item.status = ?
      AND item.assigned_thread_id IN (SELECT value FROM json_each(?))
      AND edge.parent_thread_id IN (SELECT value FROM json_each(?))
  )
            "#,
        )
        .bind(AgentJobStatus::Cancelled.as_str())
        .bind(now)
        .bind(now)
        .bind("agent job runner thread was deleted")
        .bind(AgentJobStatus::Pending.as_str())
        .bind(AgentJobStatus::Running.as_str())
        .bind(AgentJobItemStatus::Running.as_str())
        .bind(&thread_ids_json)
        .bind(&thread_ids_json)
        .execute(&mut *tx)
        .await?;
        primary_statement_count += 1;

        sqlx::query(
            "DELETE FROM thread_dynamic_tools WHERE thread_id IN (SELECT value FROM json_each(?))",
        )
        .bind(&thread_ids_json)
        .execute(&mut *tx)
        .await?;
        primary_statement_count += 1;

        sqlx::query(
            r#"
UPDATE agent_job_items
SET
    status = ?,
    assigned_thread_id = NULL,
    updated_at = ?,
    last_error = ?
WHERE assigned_thread_id IN (SELECT value FROM json_each(?)) AND status = ?
            "#,
        )
        .bind(AgentJobItemStatus::Pending.as_str())
        .bind(now)
        .bind("assigned thread was deleted")
        .bind(&thread_ids_json)
        .bind(AgentJobItemStatus::Running.as_str())
        .execute(&mut *tx)
        .await?;
        primary_statement_count += 1;

        sqlx::query(
            r#"
UPDATE agent_job_items
SET assigned_thread_id = NULL, updated_at = ?
WHERE assigned_thread_id IN (SELECT value FROM json_each(?))
            "#,
        )
        .bind(now)
        .bind(&thread_ids_json)
        .execute(&mut *tx)
        .await?;
        primary_statement_count += 1;

        sqlx::query(
            r#"
DELETE FROM thread_spawn_edges
WHERE parent_thread_id IN (SELECT value FROM json_each(?))
   OR child_thread_id IN (SELECT value FROM json_each(?))
            "#,
        )
        .bind(&thread_ids_json)
        .bind(&thread_ids_json)
        .execute(&mut *tx)
        .await?;
        primary_statement_count += 1;

        let rows_affected =
            sqlx::query("DELETE FROM threads WHERE id IN (SELECT value FROM json_each(?))")
                .bind(&thread_ids_json)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        primary_statement_count += 1;
        sqlx::query("INSERT OR IGNORE INTO deleted_thread_owners (thread_id) SELECT value FROM json_each(?)")
            .bind(&thread_ids_json).execute(&mut *tx).await?;
        primary_statement_count += 1;
        tx.commit().await?;

        if let Err(err) =
            sqlx::query("DELETE FROM logs WHERE thread_id IN (SELECT value FROM json_each(?))")
                .bind(&thread_ids_json)
                .execute(self.logs_pool.as_ref())
                .await
        {
            warn!("failed to remove logs for deleted threads: {err}");
        }

        Ok((rows_affected, primary_statement_count))
    }
}

async fn validate_thread_spawn_edge(
    connection: &mut SqliteConnection,
    parent_thread_id: ThreadId,
    child_thread_id: ThreadId,
) -> Result<(), ThreadSpawnEdgeWriteError> {
    if parent_thread_id == child_thread_id {
        return Err(ThreadSpawnEdgeWriteError::InvalidRequest {
            message: format!("thread {child_thread_id} cannot be its own parent"),
        });
    }

    let parent_is_descendant = sqlx::query_scalar::<_, i64>(
        r#"
WITH RECURSIVE descendants(child_thread_id) AS (
    SELECT child_thread_id
    FROM thread_spawn_edges
    WHERE parent_thread_id = ?
    UNION
    SELECT edge.child_thread_id
    FROM thread_spawn_edges AS edge
    JOIN descendants ON edge.parent_thread_id = descendants.child_thread_id
)
SELECT EXISTS(
    SELECT 1
    FROM descendants
    WHERE child_thread_id = ?
)
        "#,
    )
    .bind(child_thread_id.to_string())
    .bind(parent_thread_id.to_string())
    .fetch_one(connection)
    .await
    .map_err(|source| ThreadSpawnEdgeWriteError::Storage {
        operation: "validate thread-spawn edge topology",
        source,
    })? != 0;
    if parent_is_descendant {
        return Err(ThreadSpawnEdgeWriteError::InvalidRequest {
            message: format!(
                "cannot make thread {parent_thread_id} the parent of its ancestor {child_thread_id}; this would create a cycle"
            ),
        });
    }

    Ok(())
}

fn push_list_threads_query(
    builder: &mut QueryBuilder<Sqlite>,
    filters: ThreadFilterOptions<'_>,
    relation_filter: Option<crate::ThreadRelationFilter>,
    limit: usize,
) {
    if let Some(crate::ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) = relation_filter {
        builder.push(
            r#"
WITH RECURSIVE subtree(child_thread_id, parent_thread_id) AS (
    SELECT child_thread_id, parent_thread_id
    FROM thread_spawn_edges
    WHERE parent_thread_id =
"#,
        );
        builder.push_bind(ancestor_thread_id.to_string());
        builder.push(
            r#"
    UNION
    SELECT edge.child_thread_id, edge.parent_thread_id
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
)
"#,
        );
    }
    push_thread_select_columns(builder);
    // SQLite may otherwise reorder these joins and scan the global timestamp index before
    // checking the relationship. CROSS JOIN keeps the selective edge/subtree traversal first.
    match relation_filter {
        Some(crate::ThreadRelationFilter::DirectChildrenOf(_)) => builder.push(
            ", listed_edge.parent_thread_id AS parent_thread_id\nFROM thread_spawn_edges AS listed_edge\nCROSS JOIN threads ON threads.id = listed_edge.child_thread_id",
        ),
        Some(crate::ThreadRelationFilter::DescendantsOf(_)) => builder.push(
            ", subtree.parent_thread_id AS parent_thread_id\nFROM subtree\nCROSS JOIN threads ON threads.id = subtree.child_thread_id",
        ),
        None => builder.push(
            ", listed_edge.parent_thread_id AS parent_thread_id\nFROM threads\nLEFT JOIN thread_spawn_edges AS listed_edge ON listed_edge.child_thread_id = threads.id",
        ),
    };
    let include_thread_id_tiebreaker = true;
    push_thread_filters(
        builder,
        filters,
        relation_filter.is_some(),
        include_thread_id_tiebreaker,
    );
    match relation_filter {
        Some(crate::ThreadRelationFilter::DirectChildrenOf(parent_thread_id)) => {
            builder.push(" AND listed_edge.parent_thread_id = ");
            builder.push_bind(parent_thread_id.to_string());
        }
        Some(crate::ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) => {
            builder.push(" AND subtree.child_thread_id != ");
            builder.push_bind(ancestor_thread_id.to_string());
        }
        None => {}
    }
    let order_by_index = match (relation_filter, filters.cwd_filters) {
        // Relationship listings are expected to be much smaller than the global thread table.
        // Prefer the spawn-edge index and sort the matching subtree instead of scanning the
        // timestamp index until enough related threads happen to be found.
        (Some(_), _) => OrderByIndex::Disabled,
        // Multi-cwd listing is supported but at the time of writing has no current use in production.
        // Preserve its query plan so the global timestamp index does not regress cwd filtering into a scan.
        (None, Some(cwd_filters)) if cwd_filters.len() > 1 => OrderByIndex::Disabled,
        (None, Some(_) | None) => OrderByIndex::Enabled,
    };
    push_thread_order_and_limit(
        builder,
        filters.sort_key,
        filters.sort_direction,
        order_by_index,
        include_thread_id_tiebreaker,
        limit,
    );
}

pub(super) fn push_thread_select_columns(builder: &mut QueryBuilder<Sqlite>) {
    builder.push(
        r#"
SELECT
    threads.id,
    threads.session_id,
    (SELECT parent_thread_id FROM thread_spawn_edges WHERE child_thread_id = threads.id) AS current_parent_thread_id,
    threads.rollout_path,
    threads.created_at_ms AS created_at,
    threads.updated_at_ms AS updated_at,
    threads.recency_at_ms AS recency_at,
    threads.source,
    threads.history_mode,
    threads.thread_source,
    threads.agent_nickname,
    threads.agent_role,
    threads.agent_path,
    threads.model_provider,
    threads.model,
    threads.reasoning_effort,
    threads.cwd,
    threads.cli_version,
    threads.title,
    threads.preview,
    threads.sandbox_policy,
    threads.approval_mode,
    threads.tokens_used,
    threads.first_user_message,
    threads.archived_at,
    threads.project_id,
    threads.git_sha,
    threads.git_branch,
    threads.git_origin_url
"#,
    );
}

fn thread_spawn_parent_thread_id_from_source_str(source: &str) -> Option<ThreadId> {
    let parsed_source = serde_json::from_str(source)
        .or_else(|_| serde_json::from_value::<SessionSource>(Value::String(source.to_string())));
    parsed_source.ok()?.parent_thread_id()
}

#[derive(Clone, Copy)]
pub struct ThreadFilterOptions<'a> {
    pub project_id: Option<Option<&'a str>>,
    pub archived_only: bool,
    pub allowed_sources: &'a [String],
    pub model_providers: Option<&'a [String]>,
    pub cwd_filters: Option<&'a [PathBuf]>,
    pub anchor: Option<&'a crate::Anchor>,
    pub sort_key: SortKey,
    pub sort_direction: SortDirection,
    pub search_term: Option<&'a str>,
}

pub(super) fn push_thread_filters<'a>(
    builder: &mut QueryBuilder<Sqlite>,
    options: ThreadFilterOptions<'a>,
    include_empty_preview: bool,
    include_thread_id_tiebreaker: bool,
) {
    let ThreadFilterOptions {
        project_id,
        archived_only,
        allowed_sources,
        model_providers,
        cwd_filters,
        anchor,
        sort_key,
        sort_direction,
        search_term,
    } = options;
    builder.push(" WHERE 1 = 1");
    match project_id {
        Some(Some(id)) => {
            builder.push(" AND threads.project_id = ").push_bind(id);
        }
        Some(None) => {
            builder.push(" AND threads.project_id IS NULL");
        }
        None => {}
    }
    if archived_only {
        builder.push(" AND threads.archived = 1");
    } else {
        builder.push(" AND threads.archived = 0");
    }
    if !include_empty_preview {
        builder.push(" AND threads.preview <> ''");
    }
    if !allowed_sources.is_empty() {
        builder.push(" AND threads.source IN (");
        let mut separated = builder.separated(", ");
        for source in allowed_sources {
            separated.push_bind(source);
        }
        separated.push_unseparated(")");
    }
    if let Some(model_providers) = model_providers
        && !model_providers.is_empty()
    {
        builder.push(" AND threads.model_provider IN (");
        let mut separated = builder.separated(", ");
        for provider in model_providers {
            separated.push_bind(provider);
        }
        separated.push_unseparated(")");
    }
    match cwd_filters {
        Some([]) => {
            builder.push(" AND 1 = 0");
        }
        Some(cwd_filters) => {
            builder.push(" AND threads.cwd IN (");
            let mut separated = builder.separated(", ");
            for cwd in cwd_filters {
                separated.push_bind(cwd.display().to_string());
            }
            separated.push_unseparated(")");
        }
        None => {}
    }
    if let Some(search_term) = search_term {
        builder.push(" AND (instr(threads.title, ");
        builder.push_bind(search_term);
        builder.push(") > 0 OR instr(threads.preview, ");
        builder.push_bind(search_term);
        builder.push(") > 0)");
    }
    if let Some(anchor) = anchor {
        let anchor_ts = datetime_to_epoch_millis(anchor.ts);
        let column = match sort_key {
            SortKey::CreatedAt => "threads.created_at_ms",
            SortKey::UpdatedAt => "threads.updated_at_ms",
            SortKey::RecencyAt => "threads.recency_at_ms",
        };
        let operator = match sort_direction {
            SortDirection::Asc => ">",
            SortDirection::Desc => "<",
        };
        if include_thread_id_tiebreaker && let Some(anchor_id) = anchor.id {
            // A row-value range lets SQLite seek into the composite ordering
            // index. An equivalent OR predicate scans earlier pages instead.
            builder.push(" AND (");
            builder.push(column);
            builder.push(", threads.id) ");
            builder.push(operator);
            builder.push(" (");
            builder.push_bind(anchor_ts);
            builder.push(", ");
            builder.push_bind(anchor_id.to_string());
            builder.push(")");
        } else {
            builder.push(" AND ");
            builder.push(column);
            builder.push(" ");
            builder.push(operator);
            builder.push(" ");
            builder.push_bind(anchor_ts);
        }
    }
}

/// Controls whether SQLite may use the ordered column to satisfy `ORDER BY` from an index.
///
/// Disabling it adds a unary `+` to the ordered column. This preserves the sort semantics while
/// preventing a timestamp-only index from winning over a more selective filtering index.
#[derive(Clone, Copy)]
pub(super) enum OrderByIndex {
    Enabled,
    Disabled,
}

pub(super) fn push_thread_order_and_limit(
    builder: &mut QueryBuilder<Sqlite>,
    sort_key: SortKey,
    sort_direction: SortDirection,
    order_by_index: OrderByIndex,
    include_thread_id_tiebreaker: bool,
    limit: usize,
) {
    let order_column = match sort_key {
        SortKey::CreatedAt => "threads.created_at_ms",
        SortKey::UpdatedAt => "threads.updated_at_ms",
        SortKey::RecencyAt => "threads.recency_at_ms",
    };
    let order_direction = match sort_direction {
        SortDirection::Asc => "ASC",
        SortDirection::Desc => "DESC",
    };
    builder.push(" ORDER BY ");
    match order_by_index {
        OrderByIndex::Enabled => {}
        OrderByIndex::Disabled => {
            builder.push("+");
        }
    }
    builder.push(order_column);
    builder.push(" ");
    builder.push(order_direction);
    if include_thread_id_tiebreaker {
        builder.push(", threads.id ");
        builder.push(order_direction);
    }
    builder.push(" LIMIT ");
    builder.push_bind(limit as i64);
}

fn metadata_preview(metadata: &crate::ThreadMetadata) -> &str {
    metadata
        .preview
        .as_deref()
        .or(metadata.first_user_message.as_deref())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Anchor;
    use crate::DirectionalThreadSpawnEdgeStatus;
    use crate::runtime::test_support::test_thread_metadata;
    use crate::runtime::test_support::unique_temp_dir;
    use anyhow::Result;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::GitInfo;
    use codex_protocol::protocol::SessionMeta;
    use codex_protocol::protocol::SessionMetaLine;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::ThreadHistoryMode;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::path::PathBuf;

    #[tokio::test]
    async fn durable_audit_06_full_reconciliation_rejects_stale_and_deleted_bases() -> Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let id = ThreadId::new();
        let original = test_thread_metadata(home.path(), id, home.path().into());
        std::fs::write(&original.rollout_path, b"canonical source\n")?;
        let file = std::fs::metadata(&original.rollout_path)?;
        let version = (
            original.rollout_path.clone(),
            file.len(),
            file.modified().ok(),
        );
        runtime.upsert_thread(&original).await?;
        let expected = runtime.get_thread(id).await?.unwrap();
        runtime
            .patch_thread_metadata(&expected, |current| {
                current.title = "new explicit title".into();
                current.model = Some("new-model".into());
                current.tokens_used = 999;
                Ok(())
            })
            .await?;
        assert!(
            runtime
                .reconcile_thread_metadata(&original, Some(&expected), &version)
                .await
                .is_err()
        );
        let latest = runtime.get_thread(id).await?.unwrap();
        assert_eq!(latest.title, "new explicit title");
        assert_eq!(latest.model.as_deref(), Some("new-model"));
        assert_eq!(latest.tokens_used, 999);
        sqlx::query("DELETE FROM threads WHERE id = ?")
            .bind(id.to_string())
            .execute(runtime.pool.as_ref())
            .await?;
        assert!(
            runtime
                .reconcile_thread_metadata(&original, Some(&latest), &version)
                .await
                .is_err()
        );
        assert!(runtime.get_thread(id).await?.is_none());
        assert!(
            !runtime
                .patch_existing_thread_metadata(id, |current| current.rollout_path =
                    home.path().join("new"))
                .await?
        );
        runtime.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn metadata_patches_merge_against_current_row_not_stale_fallback() -> Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let id = ThreadId::new();
        let fallback = test_thread_metadata(home.path(), id, home.path().to_path_buf());
        runtime.upsert_thread(&fallback).await?;
        let (title, model) = tokio::join!(
            runtime.patch_thread_metadata(&fallback, |current| {
                current.title = "new title".into();
                Ok(())
            }),
            runtime.patch_thread_metadata(&fallback, |current| {
                current.model = Some("new model".into());
                Ok(())
            }),
        );
        title?;
        model?;
        let current = runtime.get_thread(id).await?.unwrap();
        assert_eq!(current.title, "new title");
        assert_eq!(current.model.as_deref(), Some("new model"));
        Ok(())
    }

    #[tokio::test]
    async fn batch_thread_titles_preserve_values_and_ignore_missing_ids() -> Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let first = ThreadId::new();
        let second = ThreadId::new();
        let unrelated = ThreadId::new();
        for (id, title, message) in [
            (first, "  chosen title  ", Some("first message")),
            (second, "default title", Some("default title")),
            (unrelated, "unrelated", None),
        ] {
            let mut metadata = test_thread_metadata(home.path(), id, home.path().to_path_buf());
            metadata.title = title.to_string();
            metadata.first_user_message = message.map(str::to_string);
            runtime.upsert_thread(&metadata).await?;
        }
        assert!(runtime.get_thread_titles(&[]).await?.is_empty());
        let mut rows = runtime
            .get_thread_titles(&[first, second, first, ThreadId::new()])
            .await?;
        rows.sort_by_key(|row| row.0.to_string());
        let mut expected = vec![
            (
                first,
                "  chosen title  ".to_string(),
                Some("first message".to_string()),
            ),
            (
                second,
                "default title".to_string(),
                Some("default title".to_string()),
            ),
        ];
        expected.sort_by_key(|row| row.0.to_string());
        assert_eq!(rows, expected);
        Ok(())
    }

    #[tokio::test]
    async fn builder_timestamps_round_trip_with_millisecond_and_archive_precision() -> Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let thread_id = ThreadId::new();
        let timestamp = DateTime::<Utc>::from_timestamp(1, 123_456_789).expect("timestamp");
        let mut builder = ThreadMetadataBuilder::new(
            thread_id,
            home.path().join("rollout.jsonl"),
            timestamp,
            SessionSource::Cli,
        );
        builder.cwd = home.path().to_path_buf();
        builder.updated_at = Some(timestamp);
        builder.recency_at = Some(timestamp);
        builder.archived_at = Some(timestamp);
        let metadata = builder.build("test-provider");
        assert_eq!(metadata.created_at.timestamp_millis(), 1_123);
        assert_eq!(metadata.updated_at.timestamp_millis(), 1_123);
        assert_eq!(metadata.recency_at.timestamp_millis(), 1_123);
        assert_eq!(
            metadata.archived_at.expect("archived").timestamp_millis(),
            1_000
        );
        runtime.upsert_thread(&metadata).await?;
        let stored = runtime.get_thread(thread_id).await?.expect("stored thread");
        assert_eq!(stored.created_at, metadata.created_at);
        assert_eq!(stored.updated_at, metadata.updated_at);
        assert_eq!(stored.recency_at, metadata.recency_at);
        assert_eq!(stored.archived_at, metadata.archived_at);
        Ok(())
    }

    #[tokio::test]
    async fn rollout_and_archive_updates_preserve_concurrent_metadata_writes() -> Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let thread_id = ThreadId::new();
        let metadata = test_thread_metadata(home.path(), thread_id, home.path().to_path_buf());
        let updated_rollout_path = home.path().join("updated-rollout.jsonl");
        let builder = ThreadMetadataBuilder::new(
            thread_id,
            updated_rollout_path.clone(),
            metadata.created_at,
            SessionSource::Cli,
        );
        let items = [RolloutItem::EventMsg(EventMsg::AgentMessage(
            codex_protocol::protocol::AgentMessageEvent {
                message: "completed response".into(),
                phase: None,
            },
        ))];
        for operation in 0..3 {
            let mut initial = metadata.clone();
            if operation == 2 {
                initial.archived_at = Some(metadata.updated_at);
            }
            runtime.upsert_thread(&initial).await?;
            let stored = runtime.get_thread(thread_id).await?.expect("seeded thread");
            assert_ne!(stored.rollout_path, updated_rollout_path);
            assert_eq!(stored.archived_at.is_some(), operation == 2);
            // Simulate an independent writer that has changed metadata but has
            // not committed yet. WAL readers can still see the previous row.
            let mut writer = runtime.pool.begin_with("BEGIN IMMEDIATE").await?;
            sqlx::query("UPDATE threads SET title = 'concurrent title', model = 'concurrent model' WHERE id = ?")
                .bind(thread_id.to_string()).execute(&mut *writer).await?;
            let mut update = Box::pin(async {
                match operation {
                    0 => {
                        runtime
                            .apply_rollout_items(&builder, &items, Some(metadata.updated_at))
                            .await
                    }
                    1 => {
                        runtime
                            .mark_archived(thread_id, &updated_rollout_path, metadata.updated_at)
                            .await
                    }
                    _ => {
                        runtime
                            .mark_unarchived(thread_id, &updated_rollout_path)
                            .await
                    }
                }
            });
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut update)
                    .await
                    .is_err(),
                "metadata operation must wait for the existing writer"
            );
            writer.commit().await?;
            update.await?;
            let actual = runtime
                .get_thread(thread_id)
                .await?
                .expect("thread remains stored");
            assert_eq!(actual.title, "concurrent title");
            assert_eq!(actual.model.as_deref(), Some("concurrent model"));
            assert_eq!(actual.rollout_path, updated_rollout_path);
            assert_eq!(actual.archived_at.is_some(), operation == 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn history_mode_thread_source_and_recency_persist_without_touching_other_fields()
    -> Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let thread_id = ThreadId::new();
        let metadata = test_thread_metadata(home.path(), thread_id, home.path().to_path_buf());
        runtime
            .upsert_thread_preserving_timestamps(&metadata)
            .await?;
        let before = runtime
            .get_thread(thread_id)
            .await?
            .expect("stored metadata");
        let mut changed = before.clone();
        changed.history_mode = ThreadHistoryMode::Paginated;
        changed.thread_source = Some(codex_protocol::protocol::ThreadSource::User);
        runtime
            .upsert_thread_preserving_timestamps(&changed)
            .await?;
        runtime
            .touch_thread_recency_at(thread_id, before.recency_at + chrono::Duration::seconds(5))
            .await?;
        let after = runtime
            .get_thread(thread_id)
            .await?
            .expect("updated metadata");
        assert!(after.recency_at > before.recency_at);
        assert_eq!(
            after,
            crate::ThreadMetadata {
                recency_at: after.recency_at,
                history_mode: ThreadHistoryMode::Paginated,
                thread_source: Some(codex_protocol::protocol::ThreadSource::User),
                ..before
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn delete_thread_cleans_associated_state() -> Result<()> {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string()).await?;
        let thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000401")?;
        let child_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000402")?;
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.clone(),
            ))
            .await?;
        seed_thread_cleanup_state(&runtime, thread_id, child_thread_id).await?;
        sqlx::query("INSERT INTO thread_dynamic_tools (thread_id, position, name, description, input_schema) VALUES (?, ?, ?, ?, ?)")
        .bind(thread_id.to_string())
        .bind(0_i64)
        .bind("test_tool")
        .bind("test dynamic tool")
        .bind("{}")
        .execute(runtime.pool.as_ref())
        .await?;
        runtime
            .create_agent_job(
                &AgentJobCreateParams {
                    id: "job-1".to_string(),
                    name: "test-job".to_string(),
                    instruction: "Return a result".to_string(),
                    auto_export: true,
                    max_runtime_seconds: None,
                    output_schema_json: None,
                    input_headers: vec!["path".to_string()],
                    input_csv_path: "/tmp/in.csv".to_string(),
                    output_csv_path: "/tmp/out.csv".to_string(),
                },
                &[AgentJobItemCreateParams {
                    item_id: "item-1".to_string(),
                    row_index: 0,
                    source_id: None,
                    row_json: json!({"path": "file-1"}),
                }],
            )
            .await?;
        runtime.mark_agent_job_running("job-1").await?;
        runtime
            .mark_agent_job_item_running_with_thread(
                "job-1",
                "item-1",
                &child_thread_id.to_string(),
            )
            .await?;

        let (rows, primary_statement_count) = runtime
            .delete_threads_strict_impl(&[thread_id, child_thread_id])
            .await?;

        assert_eq!(rows, 1);
        assert_eq!(primary_statement_count, 7);
        assert!(runtime.get_thread(thread_id).await?.is_none());
        let dynamic_tool_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM thread_dynamic_tools WHERE thread_id = ?")
                .bind(thread_id.to_string())
                .fetch_one(runtime.pool.as_ref())
                .await?;
        assert_eq!(dynamic_tool_count, 0);
        assert_thread_cleanup_state(&runtime, thread_id).await?;
        let job_item = runtime
            .get_agent_job_item("job-1", "item-1")
            .await?
            .expect("job item should exist");
        assert_eq!(job_item.status, AgentJobItemStatus::Pending);
        assert_eq!(job_item.assigned_thread_id, None);
        assert_eq!(
            job_item.last_error,
            Some("assigned thread was deleted".to_string())
        );
        let job = runtime
            .get_agent_job("job-1")
            .await?
            .expect("job should exist");
        assert_eq!(job.status, AgentJobStatus::Cancelled);

        let missing_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000403")?;
        let missing_child_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000404")?;
        seed_thread_cleanup_state(&runtime, missing_thread_id, missing_child_thread_id).await?;

        assert_eq!(runtime.delete_thread(missing_thread_id).await?, 0);
        assert_thread_cleanup_state(&runtime, missing_thread_id).await?;
        Ok(())
    }

    #[tokio::test]
    async fn delete_thread_keeps_retry_graph_and_auxiliary_state_on_primary_failure() -> Result<()>
    {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string()).await?;
        let thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000405")?;
        let child_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000406")?;
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.clone(),
            ))
            .await?;
        seed_thread_cleanup_state(&runtime, thread_id, child_thread_id).await?;

        sqlx::query(
            r#"
CREATE TRIGGER fail_thread_delete
BEFORE DELETE ON threads
BEGIN
    SELECT RAISE(FAIL, 'forced primary thread deletion failure');
END
            "#,
        )
        .execute(runtime.pool.as_ref())
        .await?;
        runtime
            .delete_thread(thread_id)
            .await
            .expect_err("primary deletion should fail");

        assert!(runtime.get_thread(thread_id).await?.is_some());
        assert_eq!(
            runtime.list_thread_spawn_descendants(thread_id).await?,
            vec![child_thread_id]
        );
        let logs = runtime
            .query_logs(&LogQuery {
                thread_ids: vec![thread_id.to_string()],
                ..Default::default()
            })
            .await?;
        assert_eq!(logs.len(), 1);
        Ok(())
    }

    async fn seed_thread_cleanup_state(
        runtime: &StateRuntime,
        thread_id: ThreadId,
        child_thread_id: ThreadId,
    ) -> Result<()> {
        runtime
            .upsert_thread_spawn_edge(
                thread_id,
                child_thread_id,
                DirectionalThreadSpawnEdgeStatus::Closed,
            )
            .await?;
        sqlx::query("INSERT INTO logs (ts, ts_nanos, level, target, feedback_log_body, thread_id) VALUES (1, 0, 'INFO', 'test', 'feedback log', ?)")
            .bind(thread_id.to_string())
            .execute(runtime.logs_pool.as_ref())
            .await?;
        Ok(())
    }

    async fn assert_thread_cleanup_state(
        runtime: &StateRuntime,
        thread_id: ThreadId,
    ) -> Result<()> {
        let spawn_edge_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM thread_spawn_edges WHERE parent_thread_id = ? OR child_thread_id = ?",
        )
        .bind(thread_id.to_string())
        .bind(thread_id.to_string())
        .fetch_one(runtime.pool.as_ref())
        .await?;
        assert_eq!(spawn_edge_count, 0);
        let logs = runtime
            .query_logs(&LogQuery {
                thread_ids: vec![thread_id.to_string()],
                ..Default::default()
            })
            .await?;
        assert!(logs.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn list_threads_updated_after_returns_oldest_changes_first() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let older_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("valid thread id");
        let middle_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread id");
        let newer_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000003").expect("valid thread id");
        let older_updated_at =
            DateTime::<Utc>::from_timestamp(1_700_000_100, 0).expect("valid older timestamp");
        let newer_updated_at =
            DateTime::<Utc>::from_timestamp(1_700_000_200, 0).expect("valid newer timestamp");

        for (thread_id, updated_at) in [
            (older_id, older_updated_at),
            (newer_id, newer_updated_at),
            (middle_id, newer_updated_at),
        ] {
            let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
            metadata.updated_at = updated_at;
            metadata.first_user_message = Some("hello".to_string());
            runtime
                .upsert_thread(&metadata)
                .await
                .expect("thread insert should succeed");
        }

        let anchor = Anchor {
            ts: older_updated_at,
            id: None,
        };
        let model_providers = ["test-provider".to_string()];
        let page = runtime
            .list_threads(
                /*page_size*/ 1,
                ThreadFilterOptions {
                    project_id: None,
                    archived_only: false,
                    allowed_sources: &[],
                    model_providers: Some(&model_providers),
                    cwd_filters: None,
                    anchor: Some(&anchor),
                    sort_key: SortKey::UpdatedAt,
                    sort_direction: SortDirection::Asc,
                    search_term: None,
                },
            )
            .await
            .expect("list should succeed");

        let ids = page.items.iter().map(|item| item.id).collect::<Vec<_>>();
        assert_eq!(ids, vec![newer_id]);
        assert_eq!(
            page.next_anchor,
            Some(Anchor {
                ts: DateTime::<Utc>::from_timestamp_millis(1_700_000_200_000)
                    .expect("valid timestamp"),
                id: Some(newer_id),
            })
        );

        let page = runtime
            .list_threads(
                /*page_size*/ 1,
                ThreadFilterOptions {
                    project_id: None,
                    archived_only: false,
                    allowed_sources: &[],
                    model_providers: Some(&model_providers),
                    cwd_filters: None,
                    anchor: page.next_anchor.as_ref(),
                    sort_key: SortKey::UpdatedAt,
                    sort_direction: SortDirection::Asc,
                    search_term: None,
                },
            )
            .await
            .expect("second page should succeed");

        let ids = page.items.iter().map(|item| item.id).collect::<Vec<_>>();
        assert_eq!(ids, vec![middle_id]);
        assert_eq!(page.next_anchor, None);
    }

    #[tokio::test]
    async fn list_threads_filters_by_cwd() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let first_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000101").expect("valid thread id");
        let second_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000102").expect("valid thread id");
        let other_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000103").expect("valid thread id");
        let first_cwd = codex_home.join("first");
        let second_cwd = codex_home.join("second");
        let other_cwd = codex_home.join("other");

        for (thread_id, cwd, updated_at) in [
            (first_id, first_cwd.clone(), 1_700_000_100),
            (second_id, second_cwd.clone(), 1_700_000_300),
            (other_id, other_cwd, 1_700_000_500),
        ] {
            let mut metadata = test_thread_metadata(&codex_home, thread_id, cwd);
            metadata.updated_at =
                DateTime::<Utc>::from_timestamp(updated_at, 0).expect("valid timestamp");
            runtime
                .upsert_thread(&metadata)
                .await
                .expect("thread insert should succeed");
        }

        let cwd_filters = vec![first_cwd, second_cwd];
        let first_page = runtime
            .list_threads(
                /*page_size*/ 1,
                ThreadFilterOptions {
                    project_id: None,
                    archived_only: false,
                    allowed_sources: &[],
                    model_providers: None,
                    cwd_filters: Some(cwd_filters.as_slice()),
                    anchor: None,
                    sort_key: SortKey::UpdatedAt,
                    sort_direction: SortDirection::Desc,
                    search_term: None,
                },
            )
            .await
            .expect("list should succeed");

        let ids = first_page
            .items
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![second_id]);
        assert_eq!(
            first_page.next_anchor,
            Some(Anchor {
                ts: DateTime::<Utc>::from_timestamp_millis(1_700_000_300_000)
                    .expect("valid timestamp"),
                id: Some(second_id),
            })
        );

        let second_page = runtime
            .list_threads(
                /*page_size*/ 1,
                ThreadFilterOptions {
                    project_id: None,
                    archived_only: false,
                    allowed_sources: &[],
                    model_providers: None,
                    cwd_filters: Some(cwd_filters.as_slice()),
                    anchor: first_page.next_anchor.as_ref(),
                    sort_key: SortKey::UpdatedAt,
                    sort_direction: SortDirection::Desc,
                    search_term: None,
                },
            )
            .await
            .expect("second page should succeed");

        let ids = second_page
            .items
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![first_id]);
        assert_eq!(second_page.next_anchor, None);

        let page = runtime
            .list_threads(
                /*page_size*/ 10,
                ThreadFilterOptions {
                    project_id: None,
                    archived_only: false,
                    allowed_sources: &[],
                    model_providers: None,
                    cwd_filters: Some(&[]),
                    anchor: None,
                    sort_key: SortKey::UpdatedAt,
                    sort_direction: SortDirection::Desc,
                    search_term: None,
                },
            )
            .await
            .expect("list with empty cwd filters should succeed");

        assert_eq!(page.items, Vec::new());
    }

    #[tokio::test]
    async fn list_threads_cursor_seeks_indexes_and_preserves_ties() -> Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let mut ids = Vec::new();
        for index in 0..6 {
            let id = ThreadId::from_string(&format!("00000000-0000-0000-0000-{index:012}"))?;
            let mut metadata = test_thread_metadata(home.path(), id, home.path().to_path_buf());
            let base = metadata.created_at;
            metadata.created_at = base + chrono::Duration::seconds(index / 3);
            metadata.updated_at = base + chrono::Duration::seconds((index + 3) % 6 / 3);
            metadata.recency_at = base + chrono::Duration::seconds(index % 2);
            runtime
                .upsert_thread_preserving_timestamps(&metadata)
                .await?;
            ids.push(id);
        }
        let cwd_filters = [home.path().to_path_buf()];
        for (sort_key, column) in [
            (SortKey::CreatedAt, "created_at_ms"),
            (SortKey::UpdatedAt, "updated_at_ms"),
            (SortKey::RecencyAt, "recency_at_ms"),
        ] {
            for direction in [SortDirection::Asc, SortDirection::Desc] {
                for cwd in [None, Some(cwd_filters.as_slice())] {
                    let mut anchor = None;
                    let mut listed = Vec::new();
                    loop {
                        let filters = ThreadFilterOptions {
                            project_id: None,
                            archived_only: false,
                            allowed_sources: &[],
                            model_providers: None,
                            cwd_filters: cwd,
                            anchor: anchor.as_ref(),
                            sort_key,
                            sort_direction: direction,
                            search_term: None,
                        };
                        if anchor.is_some() {
                            let mut query = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
                            push_list_threads_query(&mut query, filters, None, 2);
                            let plan = query.build().fetch_all(runtime.pool.as_ref()).await?;
                            let details = plan
                                .iter()
                                .map(|row| row.get::<String, _>("detail"))
                                .collect::<Vec<_>>();
                            assert!(
                                details
                                    .iter()
                                    .any(|detail| { detail.contains(&format!("({column},id)")) }),
                                "cursor must seek the timestamp/id range: {details:?}"
                            );
                            assert!(!details.iter().any(|detail| detail.contains("TEMP B-TREE")));
                        }
                        let page = runtime.list_threads(1, filters).await?;
                        listed.extend(page.items.iter().map(|item| item.id));
                        if listed.len() < ids.len() {
                            let last = page.items.last().expect("nonempty page");
                            let ts = match sort_key {
                                SortKey::CreatedAt => last.created_at,
                                SortKey::UpdatedAt => last.updated_at,
                                SortKey::RecencyAt => last.recency_at,
                            };
                            assert_eq!(
                                page.next_anchor,
                                Some(Anchor {
                                    ts,
                                    id: Some(last.id)
                                })
                            );
                        }
                        assert!(listed.len() <= ids.len(), "pagination must progress");
                        anchor = page.next_anchor;
                        if anchor.is_none() {
                            break;
                        }
                    }
                    let order = match sort_key {
                        SortKey::CreatedAt => [0, 1, 2, 3, 4, 5],
                        SortKey::UpdatedAt => [3, 4, 5, 0, 1, 2],
                        SortKey::RecencyAt => [0, 2, 4, 1, 3, 5],
                    };
                    let mut expected = order.map(|index| ids[index]).to_vec();
                    if direction == SortDirection::Desc {
                        expected.reverse();
                    }
                    assert_eq!(listed, expected);

                    // Legacy timestamp-only cursors exclude the entire tied bucket.
                    let first = runtime.get_thread(expected[0]).await?.expect("thread");
                    let legacy = Anchor {
                        ts: match sort_key {
                            SortKey::CreatedAt => first.created_at,
                            SortKey::UpdatedAt => first.updated_at,
                            SortKey::RecencyAt => first.recency_at,
                        },
                        id: None,
                    };
                    let page = runtime
                        .list_threads(
                            10,
                            ThreadFilterOptions {
                                project_id: None,
                                archived_only: false,
                                allowed_sources: &[],
                                model_providers: None,
                                cwd_filters: cwd,
                                anchor: Some(&legacy),
                                sort_key,
                                sort_direction: direction,
                                search_term: None,
                            },
                        )
                        .await?;
                    assert_eq!(
                        page.items.iter().map(|item| item.id).collect::<Vec<_>>(),
                        expected[3..]
                    );
                }
            }
        }
        runtime.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn list_threads_uses_indexes_matching_cwd_filters() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home, "test-provider".to_string())
            .await
            .expect("state db should initialize");

        let model_providers = ["test-provider".to_string()];
        let cwd_filters = [
            PathBuf::from("/workspace/one"),
            PathBuf::from("/workspace/two"),
        ];
        let anchor = Anchor {
            ts: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("valid timestamp"),
            id: None,
        };
        for (sort_key, visible_index, cwd_index) in [
            (
                SortKey::CreatedAt,
                "idx_threads_visible_created_at_ms",
                "idx_threads_archived_cwd_created_at_ms",
            ),
            (
                SortKey::UpdatedAt,
                "idx_threads_visible_updated_at_ms",
                "idx_threads_archived_cwd_updated_at_ms",
            ),
            (
                SortKey::RecencyAt,
                "idx_threads_visible_recency_at_ms",
                "idx_threads_archived_cwd_recency_at_ms",
            ),
        ] {
            for (cwd_filters, anchor, expected_index, expect_temp_sort) in [
                (None, None, visible_index, false),
                (Some(&cwd_filters[..1]), None, cwd_index, false),
                (
                    Some(&cwd_filters[..]),
                    None,
                    "idx_threads_archived_cwd_",
                    true,
                ),
                (Some(&cwd_filters[..]), Some(&anchor), cwd_index, true),
            ] {
                let mut builder = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
                push_list_threads_query(
                    &mut builder,
                    ThreadFilterOptions {
                        project_id: None,
                        archived_only: false,
                        allowed_sources: &[],
                        model_providers: Some(&model_providers),
                        cwd_filters,
                        anchor,
                        sort_key,
                        sort_direction: SortDirection::Desc,
                        search_term: None,
                    },
                    /*relation_filter*/ None,
                    /*limit*/ 201,
                );
                let plan_details = builder
                    .build()
                    .fetch_all(runtime.pool.as_ref())
                    .await
                    .expect("query plan should load")
                    .into_iter()
                    .map(|row| row.get::<String, _>("detail"))
                    .collect::<Vec<_>>();

                assert!(
                    plan_details
                        .iter()
                        .any(|detail| detail.contains(expected_index)),
                    "query plan did not use {expected_index}: {plan_details:?}"
                );
                assert_eq!(
                    plan_details
                        .iter()
                        .any(|detail| detail.contains("TEMP B-TREE")),
                    expect_temp_sort,
                    "unexpected sorting plan: {plan_details:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn list_threads_by_relation_filters_spawn_graph_with_keyset_pagination() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let parent_id = ThreadId::new();
        let first_child_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("valid thread id");
        let second_child_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread id");
        let grandchild_id = ThreadId::new();

        for (thread_id, created_at) in [
            (parent_id, 1_700_000_000),
            (first_child_id, 1_700_000_200),
            (second_child_id, 1_700_000_200),
            (grandchild_id, 1_700_000_300),
        ] {
            let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
            if thread_id == first_child_id {
                metadata.preview = None;
                metadata.first_user_message = None;
            }
            metadata.created_at =
                DateTime::<Utc>::from_timestamp(created_at, 0).expect("valid timestamp");
            metadata.updated_at = metadata.created_at;
            runtime
                .upsert_thread(&metadata)
                .await
                .expect("thread insert should succeed");
        }
        for (parent_thread_id, child_thread_id, status) in [
            (
                parent_id,
                first_child_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            ),
            (
                parent_id,
                second_child_id,
                DirectionalThreadSpawnEdgeStatus::Closed,
            ),
            (
                first_child_id,
                grandchild_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            ),
        ] {
            runtime
                .upsert_thread_spawn_edge(parent_thread_id, child_thread_id, status)
                .await
                .expect("spawn edge insert should succeed");
        }

        let mut builder = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
        push_list_threads_query(
            &mut builder,
            ThreadFilterOptions {
                project_id: None,
                archived_only: false,
                allowed_sources: &[],
                model_providers: None,
                cwd_filters: None,
                anchor: None,
                sort_key: SortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                search_term: None,
            },
            Some(crate::ThreadRelationFilter::DescendantsOf(parent_id)),
            /*limit*/ 10,
        );
        let plan_details = builder
            .build()
            .fetch_all(runtime.pool.as_ref())
            .await
            .expect("relationship query plan should load")
            .into_iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>();
        assert!(
            plan_details
                .iter()
                .any(|detail| detail.contains("idx_thread_spawn_edges_parent_status")),
            "spawn relationship query did not use the parent index: {plan_details:?}"
        );

        let filters = |anchor| ThreadFilterOptions {
            project_id: None,
            archived_only: false,
            allowed_sources: &[],
            model_providers: None,
            cwd_filters: None,
            anchor,
            sort_key: SortKey::CreatedAt,
            sort_direction: SortDirection::Desc,
            search_term: None,
        };
        let first_page = runtime
            .list_threads_by_relation(
                /*page_size*/ 1,
                crate::ThreadRelationFilter::DirectChildrenOf(parent_id),
                filters(None),
            )
            .await
            .expect("first page should succeed");
        let second_page = runtime
            .list_threads_by_relation(
                /*page_size*/ 1,
                crate::ThreadRelationFilter::DirectChildrenOf(parent_id),
                filters(first_page.next_anchor.as_ref()),
            )
            .await
            .expect("second page should succeed");

        assert_eq!(
            first_page
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![second_child_id]
        );
        assert_eq!(
            second_page
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![first_child_id]
        );
        assert_eq!(second_page.next_anchor, None);

        let first_descendant_page = runtime
            .list_threads_by_relation(
                /*page_size*/ 2,
                crate::ThreadRelationFilter::DescendantsOf(parent_id),
                filters(None),
            )
            .await
            .expect("first descendant page should succeed");
        let second_descendant_page = runtime
            .list_threads_by_relation(
                /*page_size*/ 2,
                crate::ThreadRelationFilter::DescendantsOf(parent_id),
                filters(first_descendant_page.next_anchor.as_ref()),
            )
            .await
            .expect("second descendant page should succeed");
        assert_eq!(
            (
                first_descendant_page
                    .items
                    .iter()
                    .map(|item| item.id)
                    .collect::<Vec<_>>(),
                second_descendant_page
                    .items
                    .iter()
                    .map(|item| item.id)
                    .collect::<Vec<_>>(),
                first_descendant_page.parent_thread_ids,
                second_descendant_page.parent_thread_ids,
                second_descendant_page.next_anchor,
            ),
            (
                vec![grandchild_id, second_child_id],
                vec![first_child_id],
                [
                    (grandchild_id, first_child_id),
                    (second_child_id, parent_id)
                ]
                .into(),
                [(first_child_id, parent_id)].into(),
                None,
            )
        );

        sqlx::query(
            "INSERT INTO thread_spawn_edges (parent_thread_id, child_thread_id, status) VALUES (?, ?, ?)",
        )
            .bind(grandchild_id.to_string())
            .bind(parent_id.to_string())
            .bind(DirectionalThreadSpawnEdgeStatus::Open.as_ref())
            .execute(runtime.pool.as_ref())
            .await
            .expect("preexisting cycle edge should insert directly");
        let cyclic_descendants = runtime
            .list_threads_by_relation(
                /*page_size*/ 10,
                crate::ThreadRelationFilter::DescendantsOf(parent_id),
                filters(None),
            )
            .await
            .expect("cyclic descendant graph should terminate");
        assert_eq!(
            cyclic_descendants
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![grandchild_id, second_child_id, first_child_id]
        );
    }

    #[tokio::test]
    async fn apply_rollout_items_preserves_existing_git_branch_and_fills_missing_git_fields() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000457").expect("valid thread id");
        let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
        metadata.git_branch = Some("sqlite-branch".to_string());

        runtime
            .upsert_thread(&metadata)
            .await
            .expect("initial upsert should succeed");

        let created_at = metadata.created_at.to_rfc3339();
        let builder = ThreadMetadataBuilder::new(
            thread_id,
            metadata.rollout_path.clone(),
            metadata.created_at,
            SessionSource::Cli,
        );
        let items = vec![RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                forked_from_id: None,
                parent_thread_id: None,
                timestamp: created_at,
                cwd: PathBuf::new(),
                originator: String::new(),
                cli_version: String::new(),
                harness_build: None,
                source: SessionSource::Cli,
                thread_source: None,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
                model_provider: None,
                base_instructions: None,
                dynamic_tools: None,
                selected_capability_roots: Vec::new(),

                history_mode: Default::default(),
                multi_agent_version: None,
                context_window: None,
            },
            git: Some(GitInfo {
                commit_hash: Some(codex_git_utils::GitSha::new("rollout-sha")),
                branch: Some("rollout-branch".to_string()),
                repository_url: Some("git@example.com:openai/codex.git".to_string()),
            }),
        })];

        runtime
            .apply_rollout_items(&builder, &items, /*updated_at_override*/ None)
            .await
            .expect("apply_rollout_items should succeed");

        let persisted = runtime
            .get_thread(thread_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(persisted.git_sha.as_deref(), Some("rollout-sha"));
        assert_eq!(persisted.git_branch.as_deref(), Some("sqlite-branch"));
        assert_eq!(
            persisted.git_origin_url.as_deref(),
            Some("git@example.com:openai/codex.git")
        );
    }

    #[tokio::test]
    async fn upsert_thread_preserves_existing_git_fields_atomically() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000458").expect("valid thread id");
        let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
        metadata.git_sha = Some("sqlite-sha".to_string());
        metadata.git_branch = Some("sqlite-branch".to_string());
        metadata.git_origin_url = Some("git@example.com:openai/codex.git".to_string());

        runtime
            .upsert_thread(&metadata)
            .await
            .expect("initial upsert should succeed");

        let mut rollout_metadata = metadata.clone();
        rollout_metadata.git_sha = Some("rollout-sha".to_string());
        rollout_metadata.git_branch = Some("rollout-branch".to_string());
        rollout_metadata.git_origin_url = Some("https://example.com/repo.git".to_string());

        runtime
            .upsert_thread(&rollout_metadata)
            .await
            .expect("rollout upsert should succeed");

        let persisted = runtime
            .get_thread(thread_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(persisted.git_sha.as_deref(), Some("sqlite-sha"));
        assert_eq!(persisted.git_branch.as_deref(), Some("sqlite-branch"));
        assert_eq!(
            persisted.git_origin_url.as_deref(),
            Some("git@example.com:openai/codex.git")
        );
    }

    #[tokio::test]
    async fn upsert_thread_preserves_existing_preview_when_incoming_preview_is_empty() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000459").expect("valid thread id");
        let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
        metadata.first_user_message = None;
        metadata.preview = Some("migrated goal preview".to_string());

        runtime
            .upsert_thread(&metadata)
            .await
            .expect("initial upsert should succeed");

        let mut rollout_metadata = metadata.clone();
        rollout_metadata.preview = None;

        runtime
            .upsert_thread(&rollout_metadata)
            .await
            .expect("rollout upsert should succeed");

        let persisted = runtime
            .get_thread(thread_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(persisted.preview.as_deref(), Some("migrated goal preview"));
    }

    #[tokio::test]
    async fn set_thread_preview_if_empty_only_fills_blank_preview() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000460").expect("valid thread id");
        let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
        metadata.first_user_message = None;
        metadata.preview = None;

        runtime
            .upsert_thread(&metadata)
            .await
            .expect("initial upsert should succeed");

        let empty_updated = runtime
            .set_thread_preview_if_empty(thread_id, "  ")
            .await
            .expect("empty preview update should succeed");
        assert!(!empty_updated);
        let goal_updated = runtime
            .set_thread_preview_if_empty(thread_id, "  goal preview  ")
            .await
            .expect("goal preview update should succeed");
        assert!(goal_updated);
        let overwrite_updated = runtime
            .set_thread_preview_if_empty(thread_id, "new preview")
            .await
            .expect("overwrite preview update should succeed");
        assert!(!overwrite_updated);

        let persisted = runtime
            .get_thread(thread_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(persisted.preview.as_deref(), Some("goal preview"));
    }

    #[tokio::test]
    async fn patch_thread_metadata_can_clear_git_fields() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000790").expect("valid thread id");
        let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
        metadata.git_sha = Some("abc123".to_string());
        metadata.git_branch = Some("feature/branch".to_string());
        metadata.git_origin_url = Some("git@example.com:openai/codex.git".to_string());

        runtime
            .upsert_thread(&metadata)
            .await
            .expect("initial upsert should succeed");

        // The upsert inside the patch keeps stored Git fields, so clearing
        // them depends on the patch's own explicit write.
        let patched = runtime
            .patch_thread_metadata(&metadata, |current| {
                current.git_sha = None;
                current.git_branch = None;
                current.git_origin_url = None;
                Ok(())
            })
            .await
            .expect("git info clear should succeed");

        let persisted = runtime
            .get_thread(thread_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(persisted.git_sha, None);
        assert_eq!(persisted.git_branch, None);
        assert_eq!(persisted.git_origin_url, None);
        assert_eq!(patched, persisted);
    }

    #[tokio::test]
    async fn touch_thread_updated_at_updates_only_updated_at() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000791").expect("valid thread id");
        let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
        metadata.title = "original title".to_string();
        metadata.first_user_message = Some("first-user-message".to_string());
        metadata.preview = None;

        runtime
            .upsert_thread(&metadata)
            .await
            .expect("initial upsert should succeed");

        let touched_at = DateTime::<Utc>::from_timestamp(1_700_001_111, 0).expect("timestamp");
        let touched = runtime
            .touch_thread_updated_at(thread_id, touched_at)
            .await
            .expect("touch should succeed");
        assert!(touched);

        let persisted = runtime
            .get_thread(thread_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(persisted.updated_at, touched_at);
        assert_eq!(persisted.title, "original title");
        assert_eq!(
            persisted.first_user_message.as_deref(),
            Some("first-user-message")
        );
        assert_eq!(persisted.preview.as_deref(), Some("first-user-message"));
    }

    #[tokio::test]
    async fn touch_thread_recency_at_is_monotonic_and_survives_stale_upsert() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000792").expect("valid thread id");
        let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
        let original_recency_at = metadata.recency_at;
        runtime
            .upsert_thread(&metadata)
            .await
            .expect("initial upsert should succeed");

        let touched_at =
            DateTime::<Utc>::from_timestamp_millis(1_700_001_111_123).expect("timestamp");
        assert!(
            runtime
                .touch_thread_recency_at(thread_id, touched_at)
                .await
                .expect("touch should succeed")
        );

        metadata.updated_at =
            DateTime::<Utc>::from_timestamp_millis(1_700_001_222_456).expect("timestamp");
        metadata.title = "updated metadata".to_string();
        assert_eq!(metadata.recency_at, original_recency_at);
        runtime
            .upsert_thread(&metadata)
            .await
            .expect("stale metadata upsert should succeed");

        let persisted = runtime
            .get_thread(thread_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(persisted.recency_at, touched_at);
        assert_eq!(persisted.updated_at, metadata.updated_at);
        assert_eq!(persisted.title, "updated metadata");

        assert!(
            runtime
                .touch_thread_recency_at(thread_id, original_recency_at)
                .await
                .expect("older touch should succeed")
        );
        let persisted = runtime
            .get_thread(thread_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(
            datetime_to_epoch_millis(persisted.recency_at),
            datetime_to_epoch_millis(touched_at) + 1
        );
    }

    #[tokio::test]
    async fn rollout_upserts_preserve_equal_persisted_timestamps() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let first_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000911").expect("valid thread id");
        let second_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000912").expect("valid thread id");
        let persisted_at =
            DateTime::<Utc>::from_timestamp_millis(1_700_001_222_123).expect("timestamp millis");
        let mut first = test_thread_metadata(&codex_home, first_id, codex_home.clone());
        first.updated_at = persisted_at;
        first.recency_at = persisted_at;
        let mut second = test_thread_metadata(&codex_home, second_id, codex_home.clone());
        second.updated_at = persisted_at;
        second.recency_at = persisted_at;

        runtime
            .upsert_thread_preserving_timestamps(&first)
            .await
            .expect("first rollout upsert should succeed");
        runtime
            .upsert_thread_preserving_timestamps(&second)
            .await
            .expect("second rollout upsert should succeed");

        for thread_id in [first_id, second_id] {
            let metadata = runtime
                .get_thread(thread_id)
                .await
                .expect("thread should load")
                .expect("thread should exist");
            assert_eq!(metadata.updated_at, persisted_at);
            assert_eq!(metadata.recency_at, persisted_at);
        }
    }

    #[test]
    fn audit20261023_allocator_remains_monotonic_after_one_thousand_hot_allocations() {
        let high = std::sync::atomic::AtomicI64::new(0);
        let base = DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).unwrap();
        let mut previous = base.timestamp_millis() - 1;
        for index in 0..1_005 {
            let allocated = super::allocate_thread_timestamp(&high, base).unwrap();
            assert_eq!(allocated.timestamp_millis(), base.timestamp_millis() + index);
            assert!(allocated.timestamp_millis() > previous);
            previous = allocated.timestamp_millis();
        }
        let rolled_back = base - chrono::Duration::seconds(60);
        assert_eq!(super::allocate_thread_timestamp(&high, rolled_back).unwrap().timestamp_millis(), previous + 1);
    }

    #[tokio::test]
    async fn thread_updated_at_uses_unique_epoch_millis_and_reads_legacy_seconds() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let first_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000901").expect("valid thread id");
        let second_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000902").expect("valid thread id");
        let older_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000903").expect("valid thread id");
        let updated_at =
            DateTime::<Utc>::from_timestamp_millis(1_700_001_111_123).expect("timestamp millis");
        let mut first = test_thread_metadata(&codex_home, first_id, codex_home.clone());
        first.updated_at = updated_at;
        first.recency_at = updated_at;
        let mut second = test_thread_metadata(&codex_home, second_id, codex_home.clone());
        second.updated_at = updated_at;
        second.recency_at = updated_at;

        runtime
            .upsert_thread(&first)
            .await
            .expect("first upsert should succeed");
        runtime
            .upsert_thread(&second)
            .await
            .expect("second upsert should succeed");

        let first = runtime
            .get_thread(first_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        let second = runtime
            .get_thread(second_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(
            datetime_to_epoch_millis(first.updated_at),
            1_700_001_111_123
        );
        assert_eq!(
            datetime_to_epoch_millis(second.updated_at),
            1_700_001_111_124
        );
        assert_eq!(
            datetime_to_epoch_millis(first.recency_at),
            1_700_001_111_123
        );
        assert_eq!(
            datetime_to_epoch_millis(second.recency_at),
            1_700_001_111_124
        );
        let second_row: (i64, i64, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT created_at, updated_at, created_at_ms, updated_at_ms FROM threads WHERE id = ?",
        )
        .bind(second_id.to_string())
        .fetch_one(runtime.pool.as_ref())
        .await
        .expect("thread timestamp row should load");
        assert_eq!(
            second_row,
            (
                datetime_to_epoch_seconds(second.created_at),
                1_700_001_111,
                Some(datetime_to_epoch_millis(second.created_at)),
                Some(1_700_001_111_124)
            )
        );

        let older_updated_at =
            DateTime::<Utc>::from_timestamp_millis(1_700_001_100_123).expect("timestamp millis");
        let mut older = test_thread_metadata(&codex_home, older_id, codex_home.clone());
        older.updated_at = older_updated_at;
        runtime
            .upsert_thread_preserving_timestamps(&older)
            .await
            .expect("older upsert should succeed");
        let older = runtime
            .get_thread(older_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(
            datetime_to_epoch_millis(older.updated_at),
            1_700_001_100_123
        );

        sqlx::query("UPDATE threads SET updated_at = ? WHERE id = ?")
            .bind(1_700_001_112_i64)
            .bind(first_id.to_string())
            .execute(runtime.pool.as_ref())
            .await
            .expect("legacy timestamp write should succeed");
        let legacy = runtime
            .get_thread(first_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(
            datetime_to_epoch_millis(legacy.updated_at),
            1_700_001_112_000
        );
    }

    #[tokio::test]
    async fn apply_rollout_items_uses_override_updated_at_when_provided() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000792").expect("valid thread id");
        let metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());

        runtime
            .upsert_thread(&metadata)
            .await
            .expect("initial upsert should succeed");

        let builder = ThreadMetadataBuilder::new(
            thread_id,
            metadata.rollout_path.clone(),
            metadata.created_at,
            SessionSource::Cli,
        );
        let items = vec![RolloutItem::EventMsg(EventMsg::TokenCount(
            codex_protocol::protocol::TokenCountEvent {
                info: Some(codex_protocol::protocol::TokenUsageInfo {
                    estimated_context_tokens: None,
                    total_token_usage: codex_protocol::protocol::TokenUsage {
                        input_tokens: 0,
                        cached_input_tokens: 0,
                        output_tokens: 0,
                        reasoning_output_tokens: 0,
                        total_tokens: 321,
                        coverage: None,
                    },
                    last_token_usage: codex_protocol::protocol::TokenUsage::default(),
                    model_context_window: None,
                }),
                rate_limits: None,
            },
        ))];
        let override_updated_at =
            DateTime::<Utc>::from_timestamp(1_700_001_234, 0).expect("timestamp");

        runtime
            .apply_rollout_items(&builder, &items, Some(override_updated_at))
            .await
            .expect("apply_rollout_items should succeed");

        let persisted = runtime
            .get_thread(thread_id)
            .await
            .expect("thread should load")
            .expect("thread should exist");
        assert_eq!(persisted.tokens_used, 321);
        assert_eq!(persisted.updated_at, override_updated_at);
    }

    #[tokio::test]
    async fn inferred_spawn_edge_skips_writer_and_rechecks_after_a_miss() -> Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let parent = ThreadId::new();
        let other_parent = ThreadId::new();
        let child = ThreadId::new();
        runtime
            .upsert_thread_spawn_edge(parent, child, DirectionalThreadSpawnEdgeStatus::Closed)
            .await?;

        // Establish the WAL reader before taking the writer lock: opening a
        // new connection itself applies initialization pragmas that can write.
        let reader = runtime.pool.acquire().await?;
        let writer = runtime.pool.begin_with("BEGIN IMMEDIATE").await?;
        drop(reader);
        tokio::time::timeout(
            Duration::from_secs(1),
            runtime.insert_thread_spawn_edge_if_absent(other_parent, child),
        )
        .await??;
        writer.rollback().await?;
        assert_eq!(
            runtime
                .list_thread_spawn_children_with_status(
                    parent,
                    DirectionalThreadSpawnEdgeStatus::Closed
                )
                .await?,
            vec![child]
        );

        // An uncommitted edge is absent from the initial WAL read. The writer
        // wins while insertion waits; the in-transaction recheck must preserve it.
        let next_child = ThreadId::new();
        let mut writer = runtime.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT INTO thread_spawn_edges VALUES (?, ?, 'closed')")
            .bind(parent.to_string())
            .bind(next_child.to_string())
            .execute(&mut *writer)
            .await?;
        let mut insert =
            Box::pin(runtime.insert_thread_spawn_edge_if_absent(other_parent, next_child));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut insert)
                .await
                .is_err()
        );
        writer.commit().await?;
        insert.await?;
        assert!(
            runtime
                .list_thread_spawn_children(other_parent)
                .await?
                .is_empty()
        );

        // The fast path does not cache existence across deletion or later calls.
        sqlx::query("DELETE FROM thread_spawn_edges WHERE child_thread_id = ?")
            .bind(child.to_string())
            .execute(runtime.pool.as_ref())
            .await?;
        runtime
            .insert_thread_spawn_edge_if_absent(other_parent, child)
            .await?;
        assert_eq!(
            runtime
                .list_thread_spawn_children_with_status(
                    other_parent,
                    DirectionalThreadSpawnEdgeStatus::Open
                )
                .await?,
            vec![child]
        );
        runtime.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn reconciled_edges_bridge_alternating_gaps_and_prefer_durable_parents() -> Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let ids = (0..7)
            .map(|n| ThreadId::from_string(&format!("00000000-0000-0000-0000-{n:012}")))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        runtime
            .upsert_thread_spawn_edge(ids[0], ids[1], DirectionalThreadSpawnEdgeStatus::Closed)
            .await?;
        runtime
            .upsert_thread_spawn_edge(ids[2], ids[3], DirectionalThreadSpawnEdgeStatus::Open)
            .await?;
        let edges = runtime
            .list_reconciled_thread_spawn_edges(
                ids[0],
                &[
                    (ids[1], ids[2]),
                    (ids[3], ids[4]),
                    (ids[0], ids[3]), // stale live parent must not compete with the durable edge
                    (ids[5], ids[6]), // unrelated component
                ],
            )
            .await?;
        assert_eq!(
            edges,
            vec![
                (ids[0], ids[1]),
                (ids[1], ids[2]),
                (ids[2], ids[3]),
                (ids[3], ids[4])
            ]
        );
        runtime.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn subtree_delete_rejects_late_edges_and_retains_explicit_deletion_evidence() -> Result<()>
    {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let root = ThreadId::new();
        let child = ThreadId::new();
        let root_meta = test_thread_metadata(home.path(), root, home.path().to_path_buf());
        let child_meta = test_thread_metadata(home.path(), child, home.path().to_path_buf());
        runtime.upsert_thread(&root_meta).await?;
        runtime.upsert_thread(&child_meta).await?;
        // The selection was [root]; a fully acknowledged edge arrived afterward.
        runtime
            .upsert_thread_spawn_edge(root, child, DirectionalThreadSpawnEdgeStatus::Open)
            .await?;
        assert!(
            runtime
                .delete_thread_subtree_strict(root, &[root])
                .await
                .is_err()
        );
        assert!(runtime.get_thread(root).await?.is_some());
        assert!(runtime.get_thread(child).await?.is_some());
        assert_eq!(runtime.list_thread_spawn_children(root).await?, vec![child]);
        runtime
            .delete_thread_subtree_strict(root, &[root, child])
            .await?;
        assert!(
            runtime.upsert_thread(&child_meta).await.is_err(),
            "delayed metadata cannot resurrect deletion"
        );
        assert!(
            runtime
                .insert_thread_spawn_edge_if_absent(root, ThreadId::new())
                .await
                .is_err()
        );
        // Missing history endpoints remain valid unless explicitly deleted.
        runtime
            .insert_thread_spawn_edge_if_absent(ThreadId::new(), ThreadId::new())
            .await?;
        runtime
            .restore_deleted_thread_owners(&[root, child])
            .await?;
        runtime.upsert_thread(&child_meta).await?;
        runtime
            .insert_thread_spawn_edge_if_absent(root, child)
            .await?;
        runtime.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn deleted_parent_does_not_fail_surviving_child_metadata_writes() -> Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let parent = ThreadId::new();
        let child = ThreadId::new();
        let parent_meta = test_thread_metadata(home.path(), parent, home.path().to_path_buf());
        let mut child_meta = test_thread_metadata(home.path(), child, home.path().to_path_buf());
        child_meta.source = crate::extract::enum_to_string(&SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                parent_thread_id: parent,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            },
        ));
        runtime.upsert_thread(&parent_meta).await?;
        runtime.upsert_thread(&child_meta).await?;
        assert_eq!(
            runtime.list_thread_spawn_children(parent).await?,
            vec![child]
        );

        // Only the parent was deleted; the surviving child keeps writing metadata.
        runtime.delete_thread(parent).await?;
        runtime.upsert_thread(&child_meta).await?;
        let renamed = runtime
            .patch_thread_metadata(&child_meta, |current| {
                current.title = "survivor".to_string();
                Ok(())
            })
            .await?;
        assert_eq!(renamed.title, "survivor");
        assert_eq!(renamed.current_parent_thread_id, None);
        assert!(runtime.list_thread_spawn_children(parent).await?.is_empty());
        assert!(
            runtime
                .insert_thread_spawn_edge_if_absent(parent, child)
                .await
                .is_err(),
            "explicit late edges for a deleted owner remain rejected"
        );
        runtime.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn deep_descendants_preserve_breadth_first_order_and_filter_barriers() -> Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let ids = (0..=1002)
            .map(|n| ThreadId::from_string(&format!("00000000-0000-0000-0000-{n:012}")))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut tx = runtime.pool.begin().await?;
        for n in 1..=1000 {
            sqlx::query("INSERT INTO thread_spawn_edges VALUES (?, ?, 'open')")
                .bind(ids[n - 1].to_string())
                .bind(ids[n].to_string())
                .execute(&mut *tx)
                .await?;
        }
        for (n, status) in [(1001, "open"), (1002, "future")] {
            sqlx::query("INSERT INTO thread_spawn_edges VALUES (?, ?, ?)")
                .bind(ids[0].to_string())
                .bind(ids[n].to_string())
                .bind(status)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        let mut expected = vec![ids[1], ids[1001], ids[1002]];
        expected.extend_from_slice(&ids[2..=1000]);
        assert_eq!(
            runtime.list_thread_spawn_descendants(ids[0]).await?,
            expected
        );
        runtime
            .set_thread_spawn_edge_status(ids[500], DirectionalThreadSpawnEdgeStatus::Closed)
            .await?;
        let mut open = vec![ids[1], ids[1001]];
        open.extend_from_slice(&ids[2..500]);
        assert_eq!(
            runtime
                .list_thread_spawn_descendants_with_status(
                    ids[0],
                    DirectionalThreadSpawnEdgeStatus::Open
                )
                .await?,
            open
        );
        assert!(
            runtime
                .list_thread_spawn_descendants_with_status(
                    ids[0],
                    DirectionalThreadSpawnEdgeStatus::Closed
                )
                .await?
                .is_empty()
        );
        runtime.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn thread_spawn_edges_track_directional_status() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home, "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let parent_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000900").expect("valid thread id");
        let child_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000901").expect("valid thread id");
        let grandchild_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000902").expect("valid thread id");

        runtime
            .upsert_thread_spawn_edge(
                parent_thread_id,
                child_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("child edge insert should succeed");
        runtime
            .upsert_thread_spawn_edge(
                child_thread_id,
                grandchild_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("grandchild edge insert should succeed");

        let children = runtime
            .list_thread_spawn_children_with_status(
                parent_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("open child list should load");
        assert_eq!(children, vec![child_thread_id]);

        let descendants = runtime
            .list_thread_spawn_descendants_with_status(
                parent_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("open descendants should load");
        assert_eq!(descendants, vec![child_thread_id, grandchild_thread_id]);

        runtime
            .set_thread_spawn_edge_status(child_thread_id, DirectionalThreadSpawnEdgeStatus::Closed)
            .await
            .expect("edge close should succeed");

        let open_children = runtime
            .list_thread_spawn_children_with_status(
                parent_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("open child list should load");
        assert_eq!(open_children, Vec::<ThreadId>::new());

        let closed_children = runtime
            .list_thread_spawn_children_with_status(
                parent_thread_id,
                DirectionalThreadSpawnEdgeStatus::Closed,
            )
            .await
            .expect("closed child list should load");
        assert_eq!(closed_children, vec![child_thread_id]);

        let closed_descendants = runtime
            .list_thread_spawn_descendants_with_status(
                parent_thread_id,
                DirectionalThreadSpawnEdgeStatus::Closed,
            )
            .await
            .expect("closed descendants should load");
        assert_eq!(closed_descendants, vec![child_thread_id]);

        let open_descendants_from_child = runtime
            .list_thread_spawn_descendants_with_status(
                child_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("open descendants from child should load");
        assert_eq!(open_descendants_from_child, vec![grandchild_thread_id]);

        let all_descendants = runtime
            .list_thread_spawn_descendants(parent_thread_id)
            .await
            .expect("all descendants should load");
        assert_eq!(all_descendants, vec![child_thread_id, grandchild_thread_id]);
    }

    #[tokio::test]
    async fn thread_spawn_edges_reject_self_parenting_and_cycles() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home, "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let first_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000920").expect("valid thread id");
        let second_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000921").expect("valid thread id");

        let self_parent_error = runtime
            .upsert_thread_spawn_edge(
                first_thread_id,
                first_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect_err("self-parent edge should be rejected");
        assert!(matches!(
            self_parent_error,
            ThreadSpawnEdgeWriteError::InvalidRequest { .. }
        ));

        let inferred_self_parent_error = runtime
            .insert_thread_spawn_edge_if_absent(first_thread_id, first_thread_id)
            .await
            .expect_err("inferred self-parent edge should be rejected");
        assert!(matches!(
            inferred_self_parent_error,
            ThreadSpawnEdgeWriteError::InvalidRequest { .. }
        ));

        runtime
            .upsert_thread_spawn_edge(
                first_thread_id,
                second_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("first edge should insert");
        let cycle_error = runtime
            .upsert_thread_spawn_edge(
                second_thread_id,
                first_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect_err("cycle-closing edge should be rejected");
        assert!(matches!(
            cycle_error,
            ThreadSpawnEdgeWriteError::InvalidRequest { .. }
        ));
        assert_eq!(
            runtime
                .list_thread_spawn_children(first_thread_id)
                .await
                .expect("children should load"),
            vec![second_thread_id]
        );
        assert_eq!(
            runtime
                .list_thread_spawn_children(second_thread_id)
                .await
                .expect("children should load"),
            Vec::<ThreadId>::new()
        );
    }

    #[tokio::test]
    async fn thread_spawn_descendant_traversal_terminates_on_preexisting_cycle() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home, "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let first_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000930").expect("valid thread id");
        let second_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000931").expect("valid thread id");

        for (parent_thread_id, child_thread_id) in [
            (first_thread_id, second_thread_id),
            (second_thread_id, first_thread_id),
        ] {
            sqlx::query(
                "INSERT INTO thread_spawn_edges (parent_thread_id, child_thread_id, status) VALUES (?, ?, ?)",
            )
            .bind(parent_thread_id.to_string())
            .bind(child_thread_id.to_string())
            .bind(DirectionalThreadSpawnEdgeStatus::Open.as_ref())
            .execute(runtime.pool.as_ref())
            .await
            .expect("malformed edge should insert directly");
        }

        let descendants = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            runtime.list_thread_spawn_descendants(first_thread_id),
        )
        .await
        .expect("cycle-safe traversal should terminate")
        .expect("descendants should load");
        assert_eq!(descendants, vec![second_thread_id]);
    }

    #[tokio::test]
    async fn thread_spawn_children_without_status_filter_lists_all_statuses() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home, "test-provider".to_string())
            .await
            .expect("state db should initialize");
        let parent_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000910").expect("valid thread id");
        let open_child_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000911").expect("valid thread id");
        let closed_child_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000912").expect("valid thread id");
        let future_child_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000913").expect("valid thread id");

        runtime
            .upsert_thread_spawn_edge(
                parent_thread_id,
                open_child_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("open child edge insert should succeed");
        runtime
            .upsert_thread_spawn_edge(
                parent_thread_id,
                closed_child_thread_id,
                DirectionalThreadSpawnEdgeStatus::Closed,
            )
            .await
            .expect("closed child edge insert should succeed");
        sqlx::query(
            r#"
INSERT INTO thread_spawn_edges (
    parent_thread_id,
    child_thread_id,
    status
) VALUES (?, ?, ?)
            "#,
        )
        .bind(parent_thread_id.to_string())
        .bind(future_child_thread_id.to_string())
        .bind("future")
        .execute(runtime.pool.as_ref())
        .await
        .expect("future-status child edge insert should succeed");

        let children = runtime
            .list_thread_spawn_children(parent_thread_id)
            .await
            .expect("all children should load");
        assert_eq!(
            children,
            vec![
                open_child_thread_id,
                closed_child_thread_id,
                future_child_thread_id,
            ]
        );
    }
}
