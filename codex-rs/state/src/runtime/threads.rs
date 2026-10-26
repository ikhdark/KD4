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

    /// Best-effort metadata hydration for a scan listing. Missing or undecodable
    /// rows are omitted independently, just as individual optional lookups are.
    pub async fn get_threads_for_listing(
        &self,
        ids: &[ThreadId],
    ) -> anyhow::Result<Vec<crate::ThreadMetadata>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = QueryBuilder::<Sqlite>::new("");
        push_thread_select_columns(&mut query);
        query.push(" FROM threads WHERE threads.id IN (SELECT value FROM json_each(");
        query.push_bind(serde_json::to_string(ids)?);
        query.push("))");
        let rows = query.build().fetch_all(self.pool.as_ref()).await?;
        Ok(rows.iter().filter_map(|row| {
            ThreadRow::try_from_row(row).and_then(ThreadMetadata::try_from).ok()
        }).collect())
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

    /// Check the child's current parent and lifecycle status using its unique incoming edge.
    pub async fn has_thread_spawn_child(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<bool> {
        let exists: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM thread_spawn_edges WHERE child_thread_id = ? AND parent_thread_id = ? AND status = ?)",
        )
        .bind(child_thread_id.to_string())
        .bind(parent_thread_id.to_string())
        .bind(status.as_ref())
        .fetch_one(self.pool.as_ref())
        .await?;
        Ok(exists != 0)
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
        self.patch_thread_metadata_with_recency(fallback, None, patch).await
    }

    /// Advance requested recency in the metadata writer's existing transaction,
    /// returning the same authoritative projection instead of rereading later.
    pub async fn patch_thread_metadata_with_recency(
        &self,
        fallback: &crate::ThreadMetadata,
        advance_recency_at: Option<DateTime<Utc>>,
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
        if let Some(recency_at) = advance_recency_at {
            let recency_at = self.allocate_thread_recency_at(recency_at)?;
            Self::touch_thread_recency_at_on_connection(&mut tx, metadata.id, recency_at).await?;
        }
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

    /// A request-local no-op observation; callers still use writer-fenced repair
    /// if the path differs. Missing rows are not recreated by this lookup.
    pub async fn thread_rollout_path_matches(&self, id: ThreadId, path: &Path) -> anyhow::Result<bool> {
        Ok(sqlx::query_scalar::<_, bool>("SELECT rollout_path = ? FROM threads WHERE id = ?")
            .bind(path.display().to_string()).bind(id.to_string())
            .fetch_optional(self.pool.as_ref()).await?.unwrap_or(false))
    }

    /// Resolve either physical spelling using one full metadata observation.
    pub async fn thread_metadata_for_rollout_path(
        &self,
        path: &Path,
    ) -> anyhow::Result<Option<crate::ThreadMetadata>> {
        let mut query = QueryBuilder::<Sqlite>::new("");
        push_thread_select_columns(&mut query);
        query.push(" FROM threads WHERE rollout_path IN (");
        query.push_bind(path.display().to_string());
        query.push(", ");
        query.push_bind(path.with_extension("jsonl.zst").display().to_string());
        query.push(") LIMIT 1");
        let row = query.build().fetch_optional(self.pool.as_ref()).await?;
        row.map(|row| ThreadRow::try_from_row(&row).and_then(ThreadMetadata::try_from))
            .transpose()
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
        let mut connection = self.pool.acquire().await?;
        Self::touch_thread_recency_at_on_connection(&mut connection, thread_id, recency_at).await
    }

    async fn touch_thread_recency_at_on_connection(
        connection: &mut sqlx::SqliteConnection,
        thread_id: ThreadId,
        recency_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
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
        .execute(connection)
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

