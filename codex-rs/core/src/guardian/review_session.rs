use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::anyhow;
use codex_analytics::GuardianReviewAnalyticsResult;
use codex_analytics::GuardianReviewSessionAnalyticsParams;
use codex_analytics::GuardianReviewSessionKind;
use codex_extension_api::UserInstructions;
use codex_protocol::ThreadId;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TokenUsage;
use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;
use tracing::warn;

use crate::codex_delegate::run_codex_thread_interactive;
use crate::config::Config;
use crate::config::Constrained;
use crate::config::ManagedFeatures;
use crate::config::NetworkProxySpec;
use crate::config::Permissions;
use crate::context::ContextualUserFragment;
use crate::context::GuardianFollowupReviewReminder;
use crate::session::Codex;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_config::types::McpServerConfig;
use codex_features::Feature;
use codex_model_provider_info::ModelProviderInfo;
use codex_utils_absolute_path::AbsolutePathBuf;

use super::GUARDIAN_REVIEWER_NAME;
use super::GuardianApprovalRequest;
use super::prompt::GuardianPromptMode;
use super::prompt::GuardianTranscriptCursor;
use super::prompt::build_guardian_prompt_items_with_parent_turn;
use super::prompt::guardian_policy_prompt;
use super::prompt::guardian_policy_prompt_with_config;
use super::review::guardian_review_session_config;

pub(crate) const GUARDIAN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
#[derive(Debug)]
pub(crate) enum GuardianReviewSessionOutcome {
    Completed(anyhow::Result<Option<String>>),
    PromptBuildFailed(anyhow::Error),
    SessionFailed {
        error: anyhow::Error,
        error_info: Option<CodexErrorInfo>,
    },
    TimedOut,
    Aborted,
}

pub(crate) struct GuardianReviewSessionParams {
    pub(crate) parent_session: Arc<Session>,
    pub(crate) parent_turn: Arc<TurnContext>,
    pub(crate) spawn_config: Config,
    pub(crate) request: Arc<GuardianApprovalRequest>,
    pub(crate) retry_reason: Option<String>,
    pub(crate) schema: Value,
    pub(crate) model: String,
    pub(crate) reasoning_effort: Option<ReasoningEffortConfig>,
    pub(crate) guardian_default_review_model_id: String,
    pub(crate) guardian_catalog_contains_auto_review: bool,
    pub(crate) guardian_review_model_overridden: bool,
    pub(crate) guardian_review_model_override: Option<String>,
    pub(crate) reasoning_summary: ReasoningSummaryConfig,
    pub(crate) personality: Option<Personality>,
    pub(crate) external_cancel: Option<CancellationToken>,
    pub(crate) deadline: tokio::time::Instant,
}

#[derive(Default)]
pub(crate) struct GuardianReviewSessionManager {
    state: Arc<Mutex<GuardianReviewSessionState>>,
    cancellation_token: CancellationToken,
    background_shutdowns: GuardianBackgroundShutdowns,
}

#[derive(Clone)]
struct GuardianBackgroundShutdowns {
    inner: Arc<GuardianBackgroundShutdownsInner>,
}

struct GuardianBackgroundShutdownsInner {
    tasks: TaskTracker,
    accepting: StdMutex<bool>,
}

impl Default for GuardianBackgroundShutdowns {
    fn default() -> Self {
        Self {
            inner: Arc::new(GuardianBackgroundShutdownsInner {
                tasks: TaskTracker::new(),
                accepting: StdMutex::new(true),
            }),
        }
    }
}

impl GuardianBackgroundShutdowns {
    fn admit_review(&self) -> Option<TaskTrackerToken> {
        let accepting = self
            .inner
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (*accepting).then(|| self.inner.tasks.token())
    }

    fn shutdown_session(&self, review_session: Arc<GuardianReviewSession>) {
        review_session.cancel_token.cancel();
        drop(self.inner.tasks.spawn(async move {
            review_session.shutdown().await;
        }));
    }

    fn cleanup_review(
        &self,
        state: Arc<Mutex<GuardianReviewSessionState>>,
        review_session: Arc<GuardianReviewSession>,
        slot: GuardianReviewSlot,
    ) {
        review_session.cancel_token.cancel();
        // An admitted review retains a tracker token until this continuation is
        // registered, including when its caller future is dropped after close.
        drop(self.inner.tasks.spawn(async move {
            let removed = match slot {
                GuardianReviewSlot::Unregistered => Some(review_session),
                GuardianReviewSlot::Trunk => {
                    let mut state = state.lock().await;
                    if state
                        .trunk
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &review_session))
                    {
                        state.trunk.take()
                    } else {
                        None
                    }
                }
                GuardianReviewSlot::Ephemeral => {
                    let mut state = state.lock().await;
                    state
                        .ephemeral_reviews
                        .iter()
                        .position(|active| Arc::ptr_eq(active, &review_session))
                        .map(|index| state.ephemeral_reviews.swap_remove(index))
                }
            };
            if let Some(review_session) = removed {
                review_session.shutdown().await;
            }
        }));
    }

    fn close(&self) {
        let mut accepting = self
            .inner
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *accepting = false;
        self.inner.tasks.close();
    }

    async fn wait(&self) {
        self.inner.tasks.wait().await;
    }
}

#[derive(Default)]
struct GuardianReviewSessionState {
    trunk: Option<Arc<GuardianReviewSession>>,
    ephemeral_reviews: Vec<Arc<GuardianReviewSession>>,
}

struct GuardianReviewSession {
    codex: Codex,
    cancel_token: CancellationToken,
    reuse_key: GuardianReviewSessionReuseKey,
    review_lock: Semaphore,
    state: Mutex<GuardianReviewState>,
}

struct GuardianReviewState {
    prior_review_count: usize,
    last_reviewed_transcript_cursor: Option<GuardianTranscriptCursor>,
    last_committed_fork_snapshot: Option<GuardianReviewForkSnapshot>,
}

fn had_prior_review_context(prompt_mode: &GuardianPromptMode) -> bool {
    matches!(prompt_mode, GuardianPromptMode::Delta { .. })
}

fn token_usage_delta(start: &TokenUsage, end: &TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: (end.input_tokens - start.input_tokens).max(0),
        cached_input_tokens: (end.cached_input_tokens - start.cached_input_tokens).max(0),
        output_tokens: (end.output_tokens - start.output_tokens).max(0),
        reasoning_output_tokens: (end.reasoning_output_tokens - start.reasoning_output_tokens)
            .max(0),
        total_tokens: (end.total_tokens - start.total_tokens).max(0),
    }
}

#[derive(Clone, Copy)]
enum GuardianReviewSlot {
    Unregistered,
    Trunk,
    Ephemeral,
}

struct ReviewSessionCleanup {
    state: Arc<Mutex<GuardianReviewSessionState>>,
    background_shutdowns: GuardianBackgroundShutdowns,
    review_session: Option<Arc<GuardianReviewSession>>,
    slot: GuardianReviewSlot,
}

#[derive(Clone)]
struct GuardianReviewForkSnapshot {
    initial_history: InitialHistory,
    prior_review_count: usize,
    last_reviewed_transcript_cursor: Option<GuardianTranscriptCursor>,
}

#[derive(Debug, Clone, PartialEq)]
struct GuardianReviewSessionReuseKey {
    // Only include settings that affect spawned-session behavior so reuse
    // invalidation remains explicit and does not depend on unrelated config
    // bookkeeping.
    model: Option<String>,
    model_provider_id: String,
    model_provider: ModelProviderInfo,
    model_context_window: Option<i64>,
    model_auto_compact_token_limit: Option<i64>,
    model_auto_compact_token_limit_scope: AutoCompactTokenLimitScope,
    model_reasoning_effort: Option<ReasoningEffortConfig>,
    model_reasoning_summary: Option<ReasoningSummaryConfig>,
    permissions: Permissions,
    developer_instructions: Option<String>,
    base_instructions: Option<String>,
    user_instructions: Option<UserInstructions>,
    compact_prompt: Option<String>,
    cwd: AbsolutePathBuf,
    mcp_servers: Constrained<HashMap<String, McpServerConfig>>,
    features: ManagedFeatures,
}

impl GuardianReviewSessionReuseKey {
    fn from_spawn_config(
        spawn_config: &Config,
        user_instructions: Option<UserInstructions>,
    ) -> Self {
        Self {
            model: spawn_config.model.clone(),
            model_provider_id: spawn_config.model_provider_id.clone(),
            model_provider: spawn_config.model_provider.clone(),
            model_context_window: spawn_config.model_context_window,
            model_auto_compact_token_limit: spawn_config.model_auto_compact_token_limit,
            model_auto_compact_token_limit_scope: spawn_config.model_auto_compact_token_limit_scope,
            model_reasoning_effort: spawn_config.model_reasoning_effort.clone(),
            model_reasoning_summary: spawn_config.model_reasoning_summary,
            permissions: spawn_config.permissions.clone(),
            developer_instructions: spawn_config.developer_instructions.clone(),
            base_instructions: spawn_config.base_instructions.clone(),
            user_instructions,
            compact_prompt: spawn_config.compact_prompt.clone(),
            cwd: spawn_config.cwd.clone(),
            mcp_servers: spawn_config.mcp_servers.clone(),
            features: spawn_config.features.clone(),
        }
    }
}

pub(crate) fn prompt_cache_key_override_for_review_session(
    session_source: &SessionSource,
    parent_thread_id: Option<ThreadId>,
) -> Option<String> {
    let SessionSource::SubAgent(SubAgentSource::Other(name)) = session_source else {
        return None;
    };
    if name != GUARDIAN_REVIEWER_NAME {
        return None;
    }
    let parent_thread_id = parent_thread_id?;
    Some(format!("guardian:{parent_thread_id}"))
}

impl GuardianReviewSession {
    async fn shutdown(&self) {
        self.cancel_token.cancel();
        let _ = self.codex.shutdown_and_wait().await;
    }

    async fn fork_snapshot(&self) -> Option<GuardianReviewForkSnapshot> {
        self.state.lock().await.last_committed_fork_snapshot.clone()
    }

    async fn refresh_last_committed_fork_snapshot(&self) {
        match load_rollout_items_for_fork(&self.codex.session).await {
            Ok(Some(items)) if !items.is_empty() => {
                let mut state = self.state.lock().await;
                let prior_review_count = state.prior_review_count;
                let last_reviewed_transcript_cursor = state.last_reviewed_transcript_cursor;
                state.last_committed_fork_snapshot = Some(GuardianReviewForkSnapshot {
                    initial_history: InitialHistory::Forked(items),
                    prior_review_count,
                    last_reviewed_transcript_cursor,
                });
            }
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(err) => {
                warn!("failed to refresh guardian trunk rollout snapshot: {err}");
            }
        }
    }
}

impl ReviewSessionCleanup {
    fn new(
        state: Arc<Mutex<GuardianReviewSessionState>>,
        background_shutdowns: GuardianBackgroundShutdowns,
        review_session: Arc<GuardianReviewSession>,
        slot: GuardianReviewSlot,
    ) -> Self {
        Self {
            state,
            background_shutdowns,
            review_session: Some(review_session),
            slot,
        }
    }

    fn disarm(&mut self) {
        self.review_session = None;
    }
}

impl Drop for ReviewSessionCleanup {
    fn drop(&mut self) {
        let Some(review_session) = self.review_session.take() else {
            return;
        };
        self.background_shutdowns.cleanup_review(
            Arc::clone(&self.state),
            review_session,
            self.slot,
        );
    }
}

impl GuardianReviewSessionManager {
    pub(crate) fn initialize(
        &self,
        parent_session: Arc<Session>,
        parent_turn: Arc<TurnContext>,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        // Boxing breaks the Session::new -> Guardian -> Session::new future recursion.
        Box::pin(async move {
            let Some(_admitted_initialization) = self.background_shutdowns.admit_review() else {
                return Ok(());
            };

            let spawn_config = guardian_review_session_config(&parent_session, &parent_turn)
                .await?
                .spawn_config;
            if self.cancellation_token.is_cancelled() {
                return Ok(());
            }
            let reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
                &spawn_config,
                parent_session.user_instructions().await,
            );
            let spawn_cancel_token = self.cancellation_token.child_token();
            let spawn_cancel_guard = spawn_cancel_token.clone().drop_guard();
            let review_session = spawn_guardian_review_session(
                &parent_session,
                &parent_turn,
                spawn_config,
                reuse_key,
                spawn_cancel_token.clone(),
                /*fork_snapshot*/ None,
            )
            .await?;
            let review_session = Arc::new(review_session);
            let mut cleanup = ReviewSessionCleanup::new(
                Arc::clone(&self.state),
                self.background_shutdowns.clone(),
                Arc::clone(&review_session),
                GuardianReviewSlot::Unregistered,
            );
            // A first review or shutdown may win while eager initialization is in flight;
            // install only if neither has happened. The cleanup owner also covers caller drop
            // while this registry lock is pending.
            let mut state = self.state.lock().await;
            if !spawn_cancel_token.is_cancelled() && state.trunk.is_none() {
                state.trunk = Some(review_session);
                cleanup.disarm();
                drop(spawn_cancel_guard.disarm());
            }
            Ok(())
        })
    }

    pub(crate) async fn trunk_rollout_path(&self) -> Option<PathBuf> {
        let trunk = self.state.lock().await.trunk.clone()?;
        trunk.codex.session.ensure_rollout_materialized().await;
        match trunk.codex.session.current_rollout_path().await {
            Ok(path) => path,
            Err(err) => {
                warn!("failed to resolve guardian trunk rollout path: {err}");
                None
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.cancellation_token.cancel();
        // Prevent a closed-and-empty interval before the registry drain is tracked.
        let handoff = self.background_shutdowns.inner.tasks.token();
        self.background_shutdowns.close();
        let state = Arc::clone(&self.state);
        let shutdowns = self.background_shutdowns.clone();
        drop(self.background_shutdowns.inner.tasks.spawn(async move {
            let (trunk, ephemeral_reviews) = {
                let mut state = state.lock().await;
                (
                    state.trunk.take(),
                    std::mem::take(&mut state.ephemeral_reviews),
                )
            };
            if let Some(trunk) = trunk {
                shutdowns.shutdown_session(trunk);
            }
            for review_session in ephemeral_reviews {
                shutdowns.shutdown_session(review_session);
            }
        }));
        drop(handoff);
        if tokio::time::timeout(GUARDIAN_SHUTDOWN_TIMEOUT, self.background_shutdowns.wait())
            .await
            .is_err()
        {
            warn!(
                timeout_secs = GUARDIAN_SHUTDOWN_TIMEOUT.as_secs(),
                "guardian reviewer shutdown exceeded its graceful deadline"
            );
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "review session selection and trunk spawning must stay serialized"
    )]
    pub(super) async fn run_review(
        &self,
        params: GuardianReviewSessionParams,
    ) -> (GuardianReviewSessionOutcome, GuardianReviewAnalyticsResult) {
        let Some(_admitted_review) = self.background_shutdowns.admit_review() else {
            return (
                GuardianReviewSessionOutcome::Aborted,
                GuardianReviewAnalyticsResult::without_session(),
            );
        };
        let deadline = params.deadline;
        let user_instructions = match run_before_review_deadline(
            deadline,
            params.external_cancel.as_ref(),
            params.parent_session.user_instructions(),
        )
        .await
        {
            Ok(instructions) => instructions,
            Err(outcome) => return (outcome, GuardianReviewAnalyticsResult::without_session()),
        };
        let next_reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            &params.spawn_config,
            user_instructions,
        );
        let mut spawned_trunk = false;
        let trunk_candidate = match run_before_review_deadline(
            deadline,
            params.external_cancel.as_ref(),
            self.state.lock(),
        )
        .await
        {
            Ok(mut state) => {
                if self.cancellation_token.is_cancelled() {
                    return (
                        GuardianReviewSessionOutcome::Aborted,
                        GuardianReviewAnalyticsResult::without_session(),
                    );
                }
                if let Some(trunk) = state.trunk.as_ref()
                    && (trunk.cancel_token.is_cancelled() || trunk.reuse_key != next_reuse_key)
                    && trunk.review_lock.try_acquire().is_ok()
                {
                    if let Some(review_session) = state.trunk.take() {
                        self.background_shutdowns.shutdown_session(review_session);
                    }
                }

                if state.trunk.is_none() {
                    let spawn_cancel_token = self.cancellation_token.child_token();
                    let review_session = match run_before_review_deadline_with_cancel(
                        deadline,
                        params.external_cancel.as_ref(),
                        &spawn_cancel_token,
                        Box::pin(spawn_guardian_review_session(
                            &params.parent_session,
                            &params.parent_turn,
                            params.spawn_config.clone(),
                            next_reuse_key.clone(),
                            spawn_cancel_token.clone(),
                            /*fork_snapshot*/ None,
                        )),
                    )
                    .await
                    {
                        Ok(Ok(review_session)) => Arc::new(review_session),
                        Ok(Err(err)) => {
                            return (
                                GuardianReviewSessionOutcome::PromptBuildFailed(err),
                                GuardianReviewAnalyticsResult::without_session(),
                            );
                        }
                        Err(outcome) => {
                            return (outcome, GuardianReviewAnalyticsResult::without_session());
                        }
                    };
                    state.trunk = Some(Arc::clone(&review_session));
                    spawned_trunk = true;
                }

                state.trunk.as_ref().cloned()
            }
            Err(outcome) => {
                return (outcome, GuardianReviewAnalyticsResult::without_session());
            }
        };

        let Some(trunk) = trunk_candidate else {
            return (
                GuardianReviewSessionOutcome::Completed(Err(anyhow!(
                    "guardian review session was not available after spawn"
                ))),
                GuardianReviewAnalyticsResult::without_session(),
            );
        };

        if trunk.reuse_key != next_reuse_key {
            return Box::pin(self.run_ephemeral_review(
                params,
                next_reuse_key,
                deadline,
                /*fork_snapshot*/ None,
            ))
            .await;
        }

        let trunk_guard = match trunk.review_lock.try_acquire() {
            Ok(trunk_guard) => trunk_guard,
            Err(_) => {
                let snapshot = match run_before_review_deadline(
                    deadline,
                    params.external_cancel.as_ref(),
                    trunk.fork_snapshot(),
                )
                .await
                {
                    Ok(snapshot) => snapshot,
                    Err(outcome) => {
                        return (outcome, GuardianReviewAnalyticsResult::without_session());
                    }
                };
                return Box::pin(self.run_ephemeral_review(
                    params,
                    next_reuse_key,
                    deadline,
                    snapshot,
                ))
                .await;
            }
        };

        let mut cleanup = ReviewSessionCleanup::new(
            Arc::clone(&self.state),
            self.background_shutdowns.clone(),
            Arc::clone(&trunk),
            GuardianReviewSlot::Trunk,
        );
        let guardian_session_kind = if spawned_trunk {
            GuardianReviewSessionKind::TrunkNew
        } else {
            GuardianReviewSessionKind::TrunkReused
        };
        let (outcome, keep_review_session, analytics_result) = Box::pin(run_review_on_session(
            trunk.as_ref(),
            &params,
            guardian_session_kind,
            deadline,
        ))
        .await;
        if keep_review_session && matches!(outcome, GuardianReviewSessionOutcome::Completed(_)) {
            if let Err(expired) = run_before_review_deadline(
                deadline,
                params.external_cancel.as_ref(),
                trunk.refresh_last_committed_fork_snapshot(),
            )
            .await
            {
                return (expired, analytics_result);
            }
        }
        if keep_review_session {
            cleanup.disarm();
        }
        drop(cleanup);
        drop(trunk_guard);
        (outcome, analytics_result)
    }

    #[cfg(test)]
    pub(crate) async fn cache_for_test(&self, codex: Codex) {
        let reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            codex.session.get_config().await.as_ref(),
            codex.session.user_instructions().await,
        );
        self.state.lock().await.trunk = Some(Arc::new(GuardianReviewSession {
            reuse_key,
            codex,
            cancel_token: CancellationToken::new(),
            review_lock: Semaphore::new(/*permits*/ 1),
            state: Mutex::new(GuardianReviewState {
                prior_review_count: 0,
                last_reviewed_transcript_cursor: None,
                last_committed_fork_snapshot: None,
            }),
        }));
    }

    #[cfg(test)]
    pub(crate) async fn retire_trunk_for_test(&self) {
        if let Some(review_session) = self.state.lock().await.trunk.take() {
            self.background_shutdowns.shutdown_session(review_session);
        }
    }

    #[cfg(test)]
    pub(crate) async fn register_ephemeral_for_test(&self, codex: Codex) {
        let reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            codex.session.get_config().await.as_ref(),
            codex.session.user_instructions().await,
        );
        self.state
            .lock()
            .await
            .ephemeral_reviews
            .push(Arc::new(GuardianReviewSession {
                reuse_key,
                codex,
                cancel_token: CancellationToken::new(),
                review_lock: Semaphore::new(/*permits*/ 1),
                state: Mutex::new(GuardianReviewState {
                    prior_review_count: 0,
                    last_reviewed_transcript_cursor: None,
                    last_committed_fork_snapshot: None,
                }),
            }));
    }

    #[cfg(test)]
    pub(crate) async fn committed_fork_rollout_items_for_test(&self) -> Option<Vec<RolloutItem>> {
        let trunk = self.state.lock().await.trunk.clone()?;
        let state = trunk.state.lock().await;
        let snapshot = state.last_committed_fork_snapshot.as_ref()?;
        match &snapshot.initial_history {
            InitialHistory::Forked(items) => Some(items.clone()),
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Resumed(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) async fn send_trunk_event_raw_for_test(&self, event: Event) {
        let trunk = self
            .state
            .lock()
            .await
            .trunk
            .clone()
            .expect("guardian trunk should exist");
        trunk.codex.session.send_event_raw(event).await;
    }

    async fn register_active_ephemeral(&self, review_session: Arc<GuardianReviewSession>) -> bool {
        let mut state = self.state.lock().await;
        if self.cancellation_token.is_cancelled() {
            return false;
        }
        state.ephemeral_reviews.push(review_session);
        true
    }

    async fn run_ephemeral_review(
        &self,
        params: GuardianReviewSessionParams,
        reuse_key: GuardianReviewSessionReuseKey,
        deadline: tokio::time::Instant,
        fork_snapshot: Option<GuardianReviewForkSnapshot>,
    ) -> (GuardianReviewSessionOutcome, GuardianReviewAnalyticsResult) {
        let spawn_cancel_token = self.cancellation_token.child_token();
        let mut fork_config = params.spawn_config.clone();
        fork_config.ephemeral = true;
        let review_session = match run_before_review_deadline_with_cancel(
            deadline,
            params.external_cancel.as_ref(),
            &spawn_cancel_token,
            Box::pin(spawn_guardian_review_session(
                &params.parent_session,
                &params.parent_turn,
                fork_config,
                reuse_key,
                spawn_cancel_token.clone(),
                fork_snapshot,
            )),
        )
        .await
        {
            Ok(Ok(review_session)) => Arc::new(review_session),
            Ok(Err(err)) => {
                return (
                    GuardianReviewSessionOutcome::PromptBuildFailed(err),
                    GuardianReviewAnalyticsResult::without_session(),
                );
            }
            Err(outcome) => {
                return (outcome, GuardianReviewAnalyticsResult::without_session());
            }
        };
        let mut cleanup = ReviewSessionCleanup::new(
            Arc::clone(&self.state),
            self.background_shutdowns.clone(),
            Arc::clone(&review_session),
            GuardianReviewSlot::Unregistered,
        );
        match run_before_review_deadline(
            deadline,
            params.external_cancel.as_ref(),
            self.register_active_ephemeral(Arc::clone(&review_session)),
        )
        .await
        {
            Ok(true) => cleanup.slot = GuardianReviewSlot::Ephemeral,
            Ok(false) => {
                return (
                    GuardianReviewSessionOutcome::Aborted,
                    GuardianReviewAnalyticsResult::without_session(),
                );
            }
            Err(outcome) => return (outcome, GuardianReviewAnalyticsResult::without_session()),
        }
        let (outcome, _, analytics_result) = Box::pin(run_review_on_session(
            review_session.as_ref(),
            &params,
            GuardianReviewSessionKind::EphemeralForked,
            deadline,
        ))
        .await;
        (outcome, analytics_result)
    }
}

async fn spawn_guardian_review_session(
    parent_session: &Arc<Session>,
    parent_turn: &Arc<TurnContext>,
    spawn_config: Config,
    reuse_key: GuardianReviewSessionReuseKey,
    cancel_token: CancellationToken,
    fork_snapshot: Option<GuardianReviewForkSnapshot>,
) -> anyhow::Result<GuardianReviewSession> {
    let (initial_history, prior_review_count, initial_transcript_cursor) = match fork_snapshot {
        Some(fork_snapshot) => (
            Some(fork_snapshot.initial_history),
            fork_snapshot.prior_review_count,
            fork_snapshot.last_reviewed_transcript_cursor,
        ),
        None => (None, 0, None),
    };
    let codex = Box::pin(run_codex_thread_interactive(
        spawn_config,
        parent_session.services.auth_manager.clone(),
        parent_session.services.models_manager.clone(),
        Arc::clone(parent_session),
        Arc::clone(parent_turn),
        cancel_token.clone(),
        SubAgentSource::Other(GUARDIAN_REVIEWER_NAME.to_string()),
        initial_history,
    ))
    .await?;

    Ok(GuardianReviewSession {
        codex,
        cancel_token,
        reuse_key,
        review_lock: Semaphore::new(/*permits*/ 1),
        state: Mutex::new(GuardianReviewState {
            prior_review_count,
            last_reviewed_transcript_cursor: initial_transcript_cursor,
            last_committed_fork_snapshot: None,
        }),
    })
}

async fn run_review_on_session(
    review_session: &GuardianReviewSession,
    params: &GuardianReviewSessionParams,
    guardian_session_kind: GuardianReviewSessionKind,
    deadline: tokio::time::Instant,
) -> (
    GuardianReviewSessionOutcome,
    bool,
    GuardianReviewAnalyticsResult,
) {
    let mut analytics_result = GuardianReviewAnalyticsResult::without_session();
    let result = run_before_review_deadline(
        deadline,
        params.external_cancel.as_ref(),
        Box::pin(run_review_on_session_inner(
            review_session,
            params,
            guardian_session_kind,
            deadline,
            &mut analytics_result,
        )),
    )
    .await;
    match result {
        Ok((outcome, keep)) => (outcome, keep, analytics_result),
        Err(outcome) => (outcome, false, analytics_result),
    }
}

async fn run_review_on_session_inner(
    review_session: &GuardianReviewSession,
    params: &GuardianReviewSessionParams,
    guardian_session_kind: GuardianReviewSessionKind,
    deadline: tokio::time::Instant,
    analytics_result: &mut GuardianReviewAnalyticsResult,
) -> (GuardianReviewSessionOutcome, bool) {
    let (send_followup_reminder, prompt_mode) = {
        let state = review_session.state.lock().await;

        let send_followup_reminder = state.prior_review_count == 1;
        let prompt_mode = if state.prior_review_count == 0 {
            GuardianPromptMode::Full
        } else if let Some(cursor) = state.last_reviewed_transcript_cursor {
            GuardianPromptMode::Delta { cursor }
        } else {
            GuardianPromptMode::Full
        };

        (send_followup_reminder, prompt_mode)
    };
    let model_info = params
        .parent_session
        .services
        .models_manager
        .get_model_info(
            params.model.as_str(),
            &params.spawn_config.to_models_manager_config(),
        )
        .await;
    let guardian_reasoning_effort = if model_info.supports_reasoning_summaries {
        params
            .reasoning_effort
            .clone()
            .or_else(|| model_info.default_reasoning_level.clone())
    } else {
        None
    };
    *analytics_result =
        GuardianReviewAnalyticsResult::from_session(GuardianReviewSessionAnalyticsParams {
            guardian_thread_id: review_session.codex.session.thread_id.to_string(),
            guardian_session_kind,
            guardian_model: params.model.clone(),
            guardian_reasoning_effort: guardian_reasoning_effort.map(|effort| effort.to_string()),
            guardian_default_review_model_id: params.guardian_default_review_model_id.clone(),
            guardian_catalog_contains_auto_review: params.guardian_catalog_contains_auto_review,
            guardian_review_model_overridden: params.guardian_review_model_overridden,
            guardian_review_model_override: params.guardian_review_model_override.clone(),
            guardian_model_provider_id: params.spawn_config.model_provider_id.clone(),
            had_prior_review_context: had_prior_review_context(&prompt_mode),
        });
    if send_followup_reminder {
        append_guardian_followup_reminder(review_session).await;
    }

    let prompt_items = run_before_review_deadline(
        deadline,
        params.external_cancel.as_ref(),
        Box::pin(async {
            params
                .parent_session
                .services
                .network_approval
                .sync_session_approved_hosts_to(
                    &review_session.codex.session.services.network_approval,
                )
                .await;

            build_guardian_prompt_items_with_parent_turn(
                params.parent_session.as_ref(),
                Some(params.parent_turn.as_ref()),
                params.retry_reason.clone(),
                params.request.as_ref(),
                prompt_mode,
            )
            .await
        }),
    )
    .await;
    let prompt_items = match prompt_items {
        Ok(prompt_items) => prompt_items,
        Err(outcome) => return (outcome, false),
    };
    let prompt_items = match prompt_items {
        Ok(prompt_items) => prompt_items,
        Err(err) => {
            return (
                GuardianReviewSessionOutcome::PromptBuildFailed(err.into()),
                false,
            );
        }
    };
    let reviewed_action_truncated = prompt_items.reviewed_action_truncated;
    analytics_result.reviewed_action_truncated = reviewed_action_truncated;
    if let Err(err) = reject_truncated_reviewed_action(reviewed_action_truncated) {
        return (GuardianReviewSessionOutcome::PromptBuildFailed(err), false);
    }
    let transcript_cursor = prompt_items.transcript_cursor;
    let token_usage_at_review_start = review_session
        .codex
        .session
        .total_token_usage()
        .await
        .unwrap_or_default();
    let guardian_permission_profile = PermissionProfile::read_only();
    let parent_turn_environments = params.parent_turn.environments.to_selections();
    // TODO(anp): Migrate guardian review thread settings to a PathUri fallback cwd so foreign
    // parent environments do not fall back to the host-native config cwd.
    let parent_turn_legacy_fallback_cwd = params
        .parent_turn
        .environments
        .primary()
        .and_then(|environment| environment.cwd().to_abs_path().ok())
        .unwrap_or_else(|| params.parent_turn.config.cwd.clone());

    let submit_result = run_before_review_deadline(
        deadline,
        params.external_cancel.as_ref(),
        Box::pin(review_session.codex.submit(Op::UserInput {
            items: prompt_items.items,
            final_output_json_schema: Some(params.schema.clone()),
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(codex_protocol::protocol::TurnEnvironmentSelections::new(
                    parent_turn_legacy_fallback_cwd,
                    parent_turn_environments,
                )),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: None,
                permission_profile: Some(guardian_permission_profile),
                summary: Some(params.reasoning_summary),
                personality: params.personality,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: params.model.clone(),
                        reasoning_effort: params.reasoning_effort.clone(),
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })),
    )
    .await;
    let child_turn_id = match submit_result {
        Ok(Ok(child_turn_id)) => child_turn_id,
        Ok(Err(err)) => {
            return (
                GuardianReviewSessionOutcome::SessionFailed {
                    error: err.into(),
                    error_info: None,
                },
                false,
            );
        }
        Err(outcome) => return (outcome, false),
    };
    let outcome = wait_for_guardian_review(
        review_session,
        child_turn_id.as_str(),
        deadline,
        params.external_cancel.as_ref(),
        analytics_result,
    )
    .await;
    if matches!(outcome.0, GuardianReviewSessionOutcome::Completed(_)) {
        if outcome.2
            && let Some(total_token_usage) = review_session.codex.session.total_token_usage().await
        {
            analytics_result.token_usage = Some(token_usage_delta(
                &token_usage_at_review_start,
                &total_token_usage,
            ));
        }
        let mut state = review_session.state.lock().await;
        state.prior_review_count = state.prior_review_count.saturating_add(1);
        state.last_reviewed_transcript_cursor = Some(transcript_cursor);
    }
    (outcome.0, outcome.1)
}

pub(super) fn reject_truncated_reviewed_action(
    reviewed_action_truncated: bool,
) -> anyhow::Result<()> {
    if reviewed_action_truncated {
        return Err(anyhow!(
            "guardian review cannot safely assess an action whose exact payload was truncated"
        ));
    }
    Ok(())
}

async fn append_guardian_followup_reminder(review_session: &GuardianReviewSession) {
    let reminder: ResponseItem = ContextualUserFragment::into(GuardianFollowupReviewReminder);
    review_session
        .codex
        .session
        .inject_no_new_turn(vec![reminder], /*current_turn_context*/ None)
        .await;
}

async fn load_rollout_items_for_fork(
    session: &Session,
) -> anyhow::Result<Option<Vec<RolloutItem>>> {
    session.try_ensure_rollout_materialized().await?;
    session.flush_rollout().await?;
    let live_thread = session.live_thread_for_persistence("guardian review fork")?;
    let history = live_thread.load_history(/*include_archived*/ true).await?;
    Ok(Some(history.items))
}

async fn wait_for_guardian_review(
    review_session: &GuardianReviewSession,
    expected_turn_id: &str,
    deadline: tokio::time::Instant,
    external_cancel: Option<&CancellationToken>,
    analytics_result: &mut GuardianReviewAnalyticsResult,
) -> (GuardianReviewSessionOutcome, bool, bool) {
    let timeout = tokio::time::sleep_until(deadline);
    tokio::pin!(timeout);
    let mut last_error: Option<ErrorEvent> = None;

    loop {
        tokio::select! {
            _ = &mut timeout => {
                return (GuardianReviewSessionOutcome::TimedOut, false, false);
            }
            _ = async {
                if let Some(cancel_token) = external_cancel {
                    cancel_token.cancelled().await;
                } else { std::future::pending::<()>().await; }
            } => {
                return (GuardianReviewSessionOutcome::Aborted, false, false);
            }
            event = review_session.codex.next_event() => {
                match event {
                    Ok(event) if !event_matches_turn(&event, expected_turn_id) => {}
                    Ok(event) => match event.msg {
                        EventMsg::TurnComplete(turn_complete) => {
                            analytics_result.tool_call_count = turn_complete
                                .timing
                                .as_ref()
                                .map(|timing| u64::from(timing.counters.tool_call_count));
                            analytics_result.time_to_first_token_ms = turn_complete
                                .time_to_first_token_ms
                                .and_then(|ms| u64::try_from(ms).ok());
                            if turn_complete.last_agent_message.is_none()
                                && let Some(error) = last_error
                            {
                                return (
                                    GuardianReviewSessionOutcome::SessionFailed {
                                        error: anyhow!(error.message),
                                        error_info: error.codex_error_info,
                                    },
                                    true,
                                    true,
                                );
                            }
                            return (
                                GuardianReviewSessionOutcome::Completed(Ok(turn_complete.last_agent_message)),
                                true,
                                true,
                            );
                        }
                        EventMsg::Error(error) => {
                            last_error = Some(error);
                        }
                        EventMsg::TurnAborted(_) => {
                            return (GuardianReviewSessionOutcome::Aborted, true, false);
                        }
                        _ => {}
                    },
                    Err(err) => {
                        return (
                            GuardianReviewSessionOutcome::Completed(Err(err.into())),
                            false,
                            false,
                        );
                    }
                }
            }
        }
    }
}

fn event_matches_turn(event: &Event, expected_turn_id: &str) -> bool {
    if event.id != expected_turn_id {
        return false;
    }

    match &event.msg {
        EventMsg::TurnComplete(turn_complete) => turn_complete.turn_id == expected_turn_id,
        EventMsg::TurnAborted(turn_aborted) => {
            turn_aborted.turn_id.as_deref() == Some(expected_turn_id)
        }
        _ => true,
    }
}

pub(crate) fn build_guardian_review_session_config(
    parent_config: &Config,
    live_network_config: Option<codex_network_proxy::NetworkProxyConfig>,
    active_model: &str,
    reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
) -> anyhow::Result<Config> {
    let mut guardian_config = parent_config.clone();
    guardian_config.model = Some(active_model.to_string());
    guardian_config.model_reasoning_effort = reasoning_effort;
    guardian_config.model_provider.request_max_retries = Some(1);
    guardian_config.model_provider.stream_max_retries = Some(1);
    guardian_config.include_skill_instructions = false;
    guardian_config.include_permissions_instructions = false;
    guardian_config.include_collaboration_mode_instructions = false;
    guardian_config.include_environment_context = false;
    guardian_config.memories.use_memories = false;
    guardian_config.memories.dedicated_tools = false;
    guardian_config.base_instructions = Some(
        parent_config
            .guardian_policy_config
            .as_deref()
            .map(guardian_policy_prompt_with_config)
            .unwrap_or_else(guardian_policy_prompt),
    );
    guardian_config.notify = None;
    guardian_config.developer_instructions = None;
    guardian_config.personality = None;
    guardian_config.permissions.approval_policy = Constrained::allow_only(AskForApproval::Never);
    guardian_config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .map_err(|err| {
            anyhow::anyhow!("guardian review session could not set permission profile: {err}")
        })?;
    guardian_config.include_apps_instructions = false;
    guardian_config
        .mcp_servers
        .set(HashMap::new())
        .map_err(|err| {
            anyhow::anyhow!("guardian review session could not clear MCP servers: {err}")
        })?;
    if let Some(live_network_config) = live_network_config
        && guardian_config.permissions.network.is_some()
    {
        let network_constraints = guardian_config
            .config_layer_stack
            .requirements()
            .network
            .as_ref()
            .map(|network| network.value.clone());
        guardian_config.permissions.network = Some(NetworkProxySpec::from_config_and_constraints(
            live_network_config,
            network_constraints,
            guardian_config.permissions.permission_profile(),
        )?);
    }
    for feature in [
        Feature::SpawnCsv,
        Feature::Collab,
        Feature::MultiAgentV2,
        Feature::CodeMode,
        Feature::CodeModeOnly,
        Feature::CodexHooks,
        Feature::Apps,
        Feature::Plugins,
        Feature::Personality,
        Feature::WebSearchRequest,
        Feature::WebSearchCached,
    ] {
        guardian_config.features.disable(feature).map_err(|err| {
            anyhow::anyhow!(
                "guardian review session could not disable `features.{}`: {err}",
                feature.key()
            )
        })?;
        if guardian_config.features.enabled(feature) {
            warn!(
                "guardian review session could not disable `features.{}`; continuing with the feature enabled",
                feature.key()
            );
        }
    }
    Ok(guardian_config)
}

pub(super) async fn run_before_review_deadline<T>(
    deadline: tokio::time::Instant,
    external_cancel: Option<&CancellationToken>,
    future: impl Future<Output = T>,
) -> Result<T, GuardianReviewSessionOutcome> {
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(deadline) => Err(GuardianReviewSessionOutcome::TimedOut),
        _ = async {
            if let Some(cancel_token) = external_cancel {
                cancel_token.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => Err(GuardianReviewSessionOutcome::Aborted),
        result = future => Ok(result),
    }
}

async fn run_before_review_deadline_with_cancel<T>(
    deadline: tokio::time::Instant,
    external_cancel: Option<&CancellationToken>,
    cancel_token: &CancellationToken,
    future: impl Future<Output = T>,
) -> Result<T, GuardianReviewSessionOutcome> {
    let cancel_on_drop = cancel_token.clone().drop_guard();
    let result = tokio::select! {
        biased;
        _ = cancel_token.cancelled() => Err(GuardianReviewSessionOutcome::Aborted),
        result = run_before_review_deadline(deadline, external_cancel, future) => result,
    };
    if result.is_ok() {
        drop(cancel_on_drop.disarm());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::AgentStatus;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::Submission;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::TurnAbortedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;

    async fn test_review_session() -> (
        GuardianReviewSession,
        async_channel::Sender<Event>,
        async_channel::Receiver<Submission>,
    ) {
        let (session, _turn, _rx) = crate::session::tests::make_session_and_context_with_rx().await;
        let (tx_sub, rx_sub) = async_channel::bounded(4);
        let (tx_event, rx_event) = async_channel::unbounded();
        let (_agent_status_tx, agent_status) =
            tokio::sync::watch::channel(AgentStatus::PendingInit);
        let reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            session.get_config().await.as_ref(),
            session.user_instructions().await,
        );

        (
            GuardianReviewSession {
                codex: Codex {
                    tx_sub,
                    rx_event,
                    agent_status,
                    session,
                    session_loop_termination: crate::session::completed_session_loop_termination(),
                },
                cancel_token: CancellationToken::new(),
                reuse_key,
                review_lock: Semaphore::new(/*permits*/ 1),
                state: Mutex::new(GuardianReviewState {
                    prior_review_count: 0,
                    last_reviewed_transcript_cursor: None,
                    last_committed_fork_snapshot: None,
                }),
            },
            tx_event,
            rx_sub,
        )
    }

    fn turn_complete_event(
        turn_id: &str,
        last_agent_message: Option<&str>,
        time_to_first_token_ms: Option<i64>,
    ) -> Event {
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                surfaced_result: None,
                turn_id: turn_id.to_string(),
                last_agent_message: last_agent_message.map(str::to_string),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms,
                timing: None,
            }),
        }
    }

    fn turn_complete_event_with_tool_count(
        turn_id: &str,
        last_agent_message: Option<&str>,
        time_to_first_token_ms: Option<i64>,
        tool_call_count: u32,
    ) -> Event {
        let mut event = turn_complete_event(turn_id, last_agent_message, time_to_first_token_ms);
        let EventMsg::TurnComplete(turn_complete) = &mut event.msg else {
            unreachable!("turn_complete_event must return a turn-complete event");
        };
        turn_complete.timing = Some(codex_protocol::protocol::TurnTiming {
            counters: codex_protocol::protocol::TurnTimingCounters {
                tool_call_count,
                ..Default::default()
            },
            ..Default::default()
        });
        event
    }

    fn turn_aborted_event(turn_id: &str) -> Event {
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(turn_id.to_string()),
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
                timing: None,
            }),
        }
    }

    async fn test_review_params() -> GuardianReviewSessionParams {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let model = turn.model_info.slug.clone();
        let reasoning_effort = turn.reasoning_effort.clone();
        let reasoning_summary = turn.reasoning_summary;
        let personality = turn.personality;
        let cwd = turn.cwd().clone();
        let spawn_config = build_guardian_review_session_config(
            turn.config.as_ref(),
            /*live_network_config*/ None,
            model.as_str(),
            reasoning_effort.clone(),
        )
        .expect("guardian config");

        GuardianReviewSessionParams {
            parent_session: Arc::new(session),
            parent_turn: Arc::new(turn),
            spawn_config,
            request: Arc::new(GuardianApprovalRequest::Shell {
                id: "shell-1".to_string(),
                command: vec!["git".to_string(), "status".to_string()],
                cwd,
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                justification: Some("Inspect repo state.".to_string()),
            }),
            retry_reason: None,
            schema: super::super::prompt::guardian_output_schema(),
            model,
            reasoning_effort,
            guardian_default_review_model_id: "codex-auto-review".to_string(),
            guardian_catalog_contains_auto_review: true,
            guardian_review_model_overridden: false,
            guardian_review_model_override: None,
            reasoning_summary,
            personality,
            external_cancel: None,
            deadline: tokio::time::Instant::now() + Duration::from_secs(30),
        }
    }

    #[tokio::test]
    async fn guardian_review_session_config_change_invalidates_cached_session() {
        let parent_config = crate::config::test_config().await;
        let cached_spawn_config = build_guardian_review_session_config(
            &parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("cached guardian config");
        let cached_reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            &cached_spawn_config,
            /*user_instructions*/ None,
        );

        let mut changed_parent_config = parent_config;
        changed_parent_config.model_provider.base_url =
            Some("https://guardian.example.invalid/v1".to_string());
        let next_spawn_config = build_guardian_review_session_config(
            &changed_parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("next guardian config");
        let next_reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            &next_spawn_config,
            /*user_instructions*/ None,
        );

        assert_ne!(cached_reuse_key, next_reuse_key);
        assert_eq!(
            cached_reuse_key,
            GuardianReviewSessionReuseKey::from_spawn_config(
                &cached_spawn_config,
                /*user_instructions*/ None,
            )
        );
    }

    #[tokio::test]
    async fn guardian_prompt_cache_key_is_scoped_to_parent_thread() {
        let session_source =
            SessionSource::SubAgent(SubAgentSource::Other(GUARDIAN_REVIEWER_NAME.to_string()));
        let parent_thread_id = ThreadId::new();
        let key =
            prompt_cache_key_override_for_review_session(&session_source, Some(parent_thread_id))
                .expect("guardian prompt cache key");

        assert_eq!(key, format!("guardian:{parent_thread_id}"));
        assert!(
            key.len() <= 64,
            "guardian prompt cache key should fit the Responses API limit"
        );
        assert_eq!(
            key,
            prompt_cache_key_override_for_review_session(&session_source, Some(parent_thread_id))
                .expect("same guardian prompt cache key")
        );
        assert_ne!(
            key,
            prompt_cache_key_override_for_review_session(&session_source, Some(ThreadId::new()))
                .expect("different parent guardian prompt cache key")
        );
        assert_eq!(
            None,
            prompt_cache_key_override_for_review_session(
                &SessionSource::Cli,
                Some(parent_thread_id)
            )
        );
        assert_eq!(
            None,
            prompt_cache_key_override_for_review_session(
                &session_source,
                /*parent_thread_id*/ None
            )
        );
    }

    #[tokio::test]
    async fn guardian_review_session_compact_scope_change_invalidates_cached_session() {
        let parent_config = crate::config::test_config().await;
        let cached_spawn_config = build_guardian_review_session_config(
            &parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("cached guardian config");
        let cached_reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            &cached_spawn_config,
            /*user_instructions*/ None,
        );

        let mut changed_parent_config = parent_config;
        changed_parent_config.model_auto_compact_token_limit_scope =
            AutoCompactTokenLimitScope::BodyAfterPrefix;
        let next_spawn_config = build_guardian_review_session_config(
            &changed_parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("next guardian config");
        let next_reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            &next_spawn_config,
            /*user_instructions*/ None,
        );

        assert_ne!(cached_reuse_key, next_reuse_key);
    }

    #[tokio::test]
    async fn guardian_review_session_config_disables_hooks() {
        let mut parent_config = crate::config::test_config().await;
        parent_config
            .features
            .enable(Feature::CodexHooks)
            .expect("enable hooks on parent config");

        let guardian_config = build_guardian_review_session_config(
            &parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("guardian config");

        assert!(!guardian_config.features.enabled(Feature::CodexHooks));
    }

    #[tokio::test]
    async fn guardian_review_session_config_uses_dedicated_prompt_context() {
        let mut parent_config = crate::config::test_config().await;
        parent_config.include_skill_instructions = true;
        parent_config.include_permissions_instructions = true;
        parent_config.include_apps_instructions = true;
        parent_config.include_collaboration_mode_instructions = true;
        parent_config.include_environment_context = true;

        let guardian_config = build_guardian_review_session_config(
            &parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("guardian config");

        assert!(!guardian_config.include_skill_instructions);
        assert!(!guardian_config.include_permissions_instructions);
        assert!(!guardian_config.include_apps_instructions);
        assert!(!guardian_config.include_collaboration_mode_instructions);
        assert!(!guardian_config.include_environment_context);
        assert_eq!(guardian_config.developer_instructions, None);
        assert_eq!(guardian_config.personality, None);
        assert!(!guardian_config.features.enabled(Feature::Personality));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_before_review_deadline_times_out_before_future_completes() {
        let outcome = run_before_review_deadline(
            tokio::time::Instant::now() + Duration::from_millis(10),
            /*external_cancel*/ None,
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
            },
        )
        .await;

        assert!(matches!(
            outcome,
            Err(GuardianReviewSessionOutcome::TimedOut)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_before_review_deadline_aborts_when_cancelled() {
        let cancel_token = CancellationToken::new();
        let canceller = cancel_token.clone();
        drop(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            canceller.cancel();
        }));

        let outcome = run_before_review_deadline(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Some(&cancel_token),
            std::future::pending::<()>(),
        )
        .await;

        assert!(matches!(
            outcome,
            Err(GuardianReviewSessionOutcome::Aborted)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_before_review_deadline_with_cancel_cancels_token_on_timeout() {
        let cancel_token = CancellationToken::new();

        let outcome = run_before_review_deadline_with_cancel(
            tokio::time::Instant::now() + Duration::from_millis(10),
            /*external_cancel*/ None,
            &cancel_token,
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
            },
        )
        .await;

        assert!(matches!(
            outcome,
            Err(GuardianReviewSessionOutcome::TimedOut)
        ));
        assert!(cancel_token.is_cancelled());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_before_review_deadline_with_cancel_cancels_token_on_abort() {
        let external_cancel = CancellationToken::new();
        let external_canceller = external_cancel.clone();
        let cancel_token = CancellationToken::new();
        drop(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            external_canceller.cancel();
        }));

        let outcome = run_before_review_deadline_with_cancel(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Some(&external_cancel),
            &cancel_token,
            std::future::pending::<()>(),
        )
        .await;

        assert!(matches!(
            outcome,
            Err(GuardianReviewSessionOutcome::Aborted)
        ));
        assert!(cancel_token.is_cancelled());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_before_review_deadline_with_cancel_preserves_token_on_success() {
        let cancel_token = CancellationToken::new();

        let outcome = run_before_review_deadline_with_cancel(
            tokio::time::Instant::now() + Duration::from_secs(1),
            /*external_cancel*/ None,
            &cancel_token,
            async { 42usize },
        )
        .await;

        assert_eq!(outcome.unwrap(), 42);
        assert!(!cancel_token.is_cancelled());
    }

    #[test]
    fn had_prior_review_context_tracks_prompt_mode() {
        assert!(!had_prior_review_context(&GuardianPromptMode::Full));
        assert!(had_prior_review_context(&GuardianPromptMode::Delta {
            cursor: GuardianTranscriptCursor {
                parent_history_version: 7,
                transcript_entry_count: 42,
            }
        }));
    }

    #[test]
    fn token_usage_delta_never_reports_negative_usage() {
        let start = TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 8,
            output_tokens: 6,
            reasoning_output_tokens: 4,
            total_tokens: 28,
        };
        let end = TokenUsage {
            input_tokens: 15,
            cached_input_tokens: 7,
            output_tokens: 10,
            reasoning_output_tokens: 2,
            total_tokens: 34,
        };

        assert_eq!(
            token_usage_delta(&start, &end),
            TokenUsage {
                input_tokens: 5,
                cached_input_tokens: 0,
                output_tokens: 4,
                reasoning_output_tokens: 0,
                total_tokens: 6,
            }
        );
    }

    #[tokio::test]
    async fn run_review_on_reused_session_waits_for_submitted_turn() {
        let (review_session, tx_event, rx_sub) = test_review_session().await;
        {
            let mut state = review_session.state.lock().await;
            state.prior_review_count = 1;
            state.last_reviewed_transcript_cursor = Some(GuardianTranscriptCursor {
                parent_history_version: 0,
                transcript_entry_count: 0,
            });
        }
        let params = test_review_params().await;

        let review = tokio::spawn(async move {
            run_review_on_session(
                &review_session,
                &params,
                GuardianReviewSessionKind::TrunkReused,
                tokio::time::Instant::now() + Duration::from_secs(1),
            )
            .await
        });
        let submission = rx_sub.recv().await.expect("guardian submission");
        tx_event
            .send(turn_complete_event("prior-turn", Some("stale"), Some(9)))
            .await
            .expect("queue prior turn completion");
        tx_event
            .send(turn_complete_event(
                submission.id.as_str(),
                Some("fresh"),
                Some(42),
            ))
            .await
            .expect("queue submitted turn completion");

        let (outcome, keep_review_session, analytics_result) =
            review.await.expect("review task should complete");
        let GuardianReviewSessionOutcome::Completed(Ok(last_agent_message)) = outcome else {
            panic!("expected submitted turn completion");
        };
        assert_eq!(last_agent_message.as_deref(), Some("fresh"));
        assert_eq!(analytics_result.time_to_first_token_ms, Some(42));
        assert!(keep_review_session);
    }

    #[tokio::test]
    async fn run_review_removes_trunk_when_event_stream_is_broken() {
        let (mut review_session, tx_event, rx_sub) = test_review_session().await;
        let params = test_review_params().await;
        review_session.reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            &params.spawn_config,
            params.parent_session.user_instructions().await,
        );
        let manager = GuardianReviewSessionManager {
            state: Arc::new(Mutex::new(GuardianReviewSessionState {
                trunk: Some(Arc::new(review_session)),
                ephemeral_reviews: Vec::new(),
            })),
            ..Default::default()
        };
        drop(tx_event);

        let (outcome, _) = manager.run_review(params).await;

        assert!(matches!(
            outcome,
            GuardianReviewSessionOutcome::Completed(Err(_))
        ));
        let submitted = rx_sub.recv().await.expect("normal review submission");
        assert!(matches!(submitted.op, Op::UserInput { .. }));
        let shutdown = tokio::time::timeout(Duration::from_secs(1), rx_sub.recv())
            .await
            .expect("broken stream must retire the actual child")
            .expect("shutdown submission");
        assert!(matches!(shutdown.op, Op::Shutdown));
        assert!(manager.state.lock().await.trunk.is_none());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn wait_for_guardian_review_ignores_prior_turn_completion() {
        let (review_session, tx_event, _rx_sub) = test_review_session().await;
        tx_event
            .send(turn_complete_event("prior-turn", Some("stale"), Some(9)))
            .await
            .expect("queue prior turn completion");
        tx_event
            .send(turn_complete_event_with_tool_count(
                "current-turn",
                Some("fresh"),
                Some(42),
                3,
            ))
            .await
            .expect("queue current turn completion");

        let mut analytics_result = GuardianReviewAnalyticsResult::without_session();
        let (outcome, keep_review_session, capture_token_usage) = wait_for_guardian_review(
            &review_session,
            "current-turn",
            tokio::time::Instant::now() + Duration::from_secs(1),
            /*external_cancel*/ None,
            &mut analytics_result,
        )
        .await;

        let GuardianReviewSessionOutcome::Completed(Ok(last_agent_message)) = outcome else {
            panic!("expected current turn completion");
        };
        assert_eq!(last_agent_message.as_deref(), Some("fresh"));
        assert_eq!(analytics_result.time_to_first_token_ms, Some(42));
        assert_eq!(analytics_result.tool_call_count, Some(3));
        assert!(keep_review_session);
        assert!(capture_token_usage);
    }

    #[tokio::test]
    async fn wait_for_guardian_review_ignores_prior_turn_errors() {
        let (review_session, tx_event, _rx_sub) = test_review_session().await;
        tx_event
            .send(Event {
                id: "prior-turn".to_string(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "stale guardian error".to_string(),
                    codex_error_info: None,
                }),
            })
            .await
            .expect("queue prior turn error");
        tx_event
            .send(turn_complete_event(
                "current-turn",
                /*last_agent_message*/ None,
                Some(42),
            ))
            .await
            .expect("queue current turn completion");

        let mut analytics_result = GuardianReviewAnalyticsResult::without_session();
        let (outcome, keep_review_session, capture_token_usage) = wait_for_guardian_review(
            &review_session,
            "current-turn",
            tokio::time::Instant::now() + Duration::from_secs(1),
            /*external_cancel*/ None,
            &mut analytics_result,
        )
        .await;

        let GuardianReviewSessionOutcome::Completed(Ok(last_agent_message)) = outcome else {
            panic!("expected current turn completion");
        };
        assert_eq!(last_agent_message, None);
        assert_eq!(analytics_result.time_to_first_token_ms, Some(42));
        assert!(keep_review_session);
        assert!(capture_token_usage);
    }

    #[tokio::test]
    async fn wait_for_guardian_review_preserves_structured_session_error() {
        let (review_session, tx_event, _rx_sub) = test_review_session().await;
        tx_event
            .send(Event {
                id: "current-turn".to_string(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "temporary failure".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ServerOverloaded),
                }),
            })
            .await
            .expect("queue guardian error");
        tx_event
            .send(turn_complete_event(
                "current-turn",
                /*last_agent_message*/ None,
                Some(42),
            ))
            .await
            .expect("queue current turn completion");

        let mut analytics_result = GuardianReviewAnalyticsResult::without_session();
        let (outcome, keep_review_session, capture_token_usage) = wait_for_guardian_review(
            &review_session,
            "current-turn",
            tokio::time::Instant::now() + Duration::from_secs(1),
            /*external_cancel*/ None,
            &mut analytics_result,
        )
        .await;

        let GuardianReviewSessionOutcome::SessionFailed { error, error_info } = outcome else {
            panic!("expected structured session failure");
        };
        assert_eq!(error.to_string(), "temporary failure");
        assert_eq!(error_info, Some(CodexErrorInfo::ServerOverloaded));
        assert!(keep_review_session);
        assert!(capture_token_usage);
    }

    #[tokio::test]
    async fn wait_for_guardian_review_ignores_prior_turn_aborts() {
        let (review_session, tx_event, _rx_sub) = test_review_session().await;
        tx_event
            .send(turn_aborted_event("prior-turn"))
            .await
            .expect("queue prior turn abort");
        tx_event
            .send(turn_complete_event("current-turn", Some("fresh"), Some(42)))
            .await
            .expect("queue current turn completion");

        let mut analytics_result = GuardianReviewAnalyticsResult::without_session();
        let (outcome, keep_review_session, capture_token_usage) = wait_for_guardian_review(
            &review_session,
            "current-turn",
            tokio::time::Instant::now() + Duration::from_secs(1),
            /*external_cancel*/ None,
            &mut analytics_result,
        )
        .await;

        let GuardianReviewSessionOutcome::Completed(Ok(last_agent_message)) = outcome else {
            panic!("expected current turn completion");
        };
        assert_eq!(last_agent_message.as_deref(), Some("fresh"));
        assert_eq!(analytics_result.time_to_first_token_ms, Some(42));
        assert!(keep_review_session);
        assert!(capture_token_usage);
    }
    #[test]
    fn real_ephemeral_review_timeout_and_cancel_retire_only_the_ephemeral_session()
    -> anyhow::Result<()> {
        // Match the existing real parallel guardian fixture's stack budget and single clock owner.
        std::thread::Builder::new()
        .name("guardian-real-ephemeral-cleanup".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| -> anyhow::Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(Box::pin(async {
                use core_test_support::responses::ev_assistant_message;
                use core_test_support::responses::ev_completed;
                use core_test_support::responses::ev_response_created;
                use core_test_support::responses::sse;
                use core_test_support::streaming_sse::StreamingSseChunk;
                use core_test_support::streaming_sse::start_streaming_sse_server;
                use codex_protocol::models::ContentItem;
                use tokio::time::Instant;

                async fn params(
                    session: &Arc<Session>, turn: &Arc<TurnContext>, id: &str,
                    deadline: Instant, external_cancel: CancellationToken,
                ) -> anyhow::Result<GuardianReviewSessionParams> {
                    let config = guardian_review_session_config(session, turn).await?;
                    Ok(GuardianReviewSessionParams {
                        parent_session: Arc::clone(session), parent_turn: Arc::clone(turn),
                        spawn_config: config.spawn_config,
                        request: Arc::new(GuardianApprovalRequest::Shell {
                            id: id.to_string(), command: vec!["git".to_string(), "status".to_string()],
                            cwd: turn.config.cwd.clone(),
                            sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                            additional_permissions: None, justification: Some("Inspect repository status.".to_string()),
                        }),
                        retry_reason: None, schema: crate::guardian::prompt::guardian_output_schema(),
                        model: config.model, reasoning_effort: config.reasoning_effort,
                        guardian_default_review_model_id: config.default_review_model_id,
                        guardian_catalog_contains_auto_review: config.catalog_contains_auto_review,
                        guardian_review_model_overridden: config.model_overridden,
                        guardian_review_model_override: config.model_override,
                        reasoning_summary: turn.reasoning_summary, personality: turn.personality,
                        external_cancel: Some(external_cancel), deadline,
                    })
                }

                for expire_deadline in [false, true] {
                    let assessment = serde_json::json!({"risk_level":"low", "user_authorization":"high", "outcome":"allow", "rationale":"Read-only repository inspection is allowed."}).to_string();
                    let (trunk_release, trunk_gate) = tokio::sync::oneshot::channel();
                    let (ephemeral_release, ephemeral_gate) = tokio::sync::oneshot::channel();
                    let (server, _) = start_streaming_sse_server(vec![
                        vec![
                            StreamingSseChunk { gate: None, body: sse(vec![ev_response_created("real-trunk")]) },
                            StreamingSseChunk { gate: Some(trunk_gate), body: sse(vec![ev_assistant_message("trunk-message", &assessment), ev_completed("real-trunk")]) },
                        ],
                        vec![
                            StreamingSseChunk { gate: None, body: sse(vec![ev_response_created("real-ephemeral")]) },
                            StreamingSseChunk { gate: Some(ephemeral_gate), body: sse(vec![ev_assistant_message("ephemeral-message", &assessment), ev_completed("real-ephemeral")]) },
                        ],
                    ]).await;
                    let (mut parent_session, mut parent_turn) = Box::pin(crate::session::tests::make_session_and_context()).await;
                    let mut config = (*parent_turn.config).clone();
                    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                    config.model_provider.supports_websockets = false;
                    parent_session.services.models_manager = crate::test_support::models_manager_with_provider(
                        config.codex_home.to_path_buf(), Arc::clone(&parent_session.services.auth_manager), config.model_provider.clone(),
                    );
                    parent_turn.provider = codex_model_provider::create_model_provider(config.model_provider.clone(), parent_turn.auth_manager.clone());
                    parent_turn.config = Arc::new(config);
                    let session = Arc::new(parent_session);
                    let turn = Arc::new(parent_turn);
                    session.record_conversation_items(&turn, &[ResponseItem::Message {
                        id: None, role: "user".to_string(), content: vec![ContentItem::InputText { text: "Please inspect repository status.".to_string() }],
                        phase: None, internal_chat_message_metadata_passthrough: None,
                    }]).await;
                    let trunk_params = params(&session, &turn, "held-trunk", Instant::now() + Duration::from_secs(3600), CancellationToken::new()).await?;
                    let trunk_session = Arc::clone(&session);
                    let mut trunk_review = tokio::spawn(async move { trunk_session.guardian_review_session.run_review(trunk_params).await });
                    tokio::select! {
                        request = tokio::time::timeout(Duration::from_secs(10), server.wait_for_request_count(1)) => {
                            request.expect("real trunk model request");
                        }
                        result = &mut trunk_review => {
                            let (outcome, _) = result.expect("trunk review task must not panic before request");
                            panic!("trunk review ended before real model request: {outcome:?}");
                        }
                    }
                    let (trunk_id, trunk_cancel) = {
                        let state = session.guardian_review_session.state.lock().await;
                        let trunk = state.trunk.as_ref().expect("normal manager registered real trunk");
                        (trunk.codex.session.thread_id, trunk.cancel_token.clone())
                    };
                    let external_cancel = CancellationToken::new();
                    let ephemeral_deadline = Instant::now() + Duration::from_secs(90);
                    let ephemeral_params = params(&session, &turn, "held-ephemeral", ephemeral_deadline, external_cancel.clone()).await?;
                    let ephemeral_session = Arc::clone(&session);
                    let mut ephemeral_review = tokio::spawn(async move { ephemeral_session.guardian_review_session.run_review(ephemeral_params).await });
                    tokio::select! {
                        request = tokio::time::timeout(Duration::from_secs(10), server.wait_for_request_count(2)) => {
                            request.expect("busy trunk forces real spawned ephemeral model request");
                        }
                        result = &mut ephemeral_review => {
                            let (outcome, _) = result.expect("ephemeral review task must not panic before request");
                            panic!("ephemeral review ended before real model request: {outcome:?}");
                        }
                    }
                    let (ephemeral_id, ephemeral_cancel, ephemeral_stopped, ephemeral_status) = {
                        let state = session.guardian_review_session.state.lock().await;
                        assert_eq!(state.ephemeral_reviews.len(), 1);
                        let ephemeral = &state.ephemeral_reviews[0];
                        (ephemeral.codex.session.thread_id, ephemeral.cancel_token.clone(), ephemeral.codex.session_loop_termination.clone(), ephemeral.codex.agent_status.clone())
                    };
                    assert_ne!(ephemeral_id, trunk_id, "ephemeral must be a distinct spawned Codex session");
                    assert!(!ephemeral_cancel.is_cancelled());
                    if expire_deadline {
                        tokio::time::pause();
                        tokio::time::advance(ephemeral_deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1)).await;
                    } else {
                        external_cancel.cancel();
                    }
                    let (outcome, analytics) = tokio::time::timeout(Duration::from_secs(1), ephemeral_review).await
                        .expect("review result must not wait for old five-second interrupt drain")
                        .expect("ephemeral review task");
                    if expire_deadline { tokio::time::resume(); }
                    if expire_deadline { assert!(matches!(outcome, GuardianReviewSessionOutcome::TimedOut)); }
                    else { assert!(matches!(outcome, GuardianReviewSessionOutcome::Aborted)); }
                    assert_eq!(analytics.guardian_thread_id.as_deref(), Some(ephemeral_id.to_string().as_str()));
                    assert!(matches!(analytics.guardian_session_kind, Some(GuardianReviewSessionKind::EphemeralForked)));
                    assert!(ephemeral_cancel.is_cancelled(), "retirement must signal the real session immediately");
                    // Observe the actual submission-loop completion, not a cleanup helper or registry-only flag.
                    tokio::time::timeout(Duration::from_secs(5), ephemeral_stopped).await.expect("real ephemeral session must terminate");
                    assert_eq!(*ephemeral_status.borrow(), AgentStatus::Shutdown);
                    {
                        let state = session.guardian_review_session.state.lock().await;
                        assert!(state.ephemeral_reviews.is_empty(), "retired real session must leave active registry");
                        assert_eq!(state.trunk.as_ref().expect("parallel trunk retained").codex.session.thread_id, trunk_id);
                    }
                    assert!(!trunk_cancel.is_cancelled(), "ephemeral retirement must preserve the parallel trunk");
                    assert!(tokio::time::timeout(Duration::from_millis(20), &mut trunk_review).await.is_err(), "trunk still awaits its own model response");
                    assert_eq!(server.requests().await.len(), 2, "cancel/timeout must not launch a replacement review");
                    // The positive sibling control completes through the same real manager and HTTP path.
                    trunk_release.send(()).expect("release unaffected trunk response");
                    let (trunk_outcome, trunk_analytics) = tokio::time::timeout(Duration::from_secs(10), trunk_review).await.expect("trunk completes").expect("trunk review task");
                    assert!(matches!(trunk_outcome, GuardianReviewSessionOutcome::Completed(Ok(Some(message))) if message == assessment));
                    assert_eq!(trunk_analytics.guardian_thread_id.as_deref(), Some(trunk_id.to_string().as_str()));
                    drop(ephemeral_release);
                    session.guardian_review_session.shutdown().await;
                    server.shutdown().await;
                }
                Ok(())
            }))
        })?.join().map_err(|_| anyhow!("real ephemeral cleanup test panicked"))?
    }

    #[tokio::test]
    async fn guardian_review_deadline_does_not_wait_for_retirement_registry_or_child_shutdown() {
        let (mut child, tx_event, rx_sub) = test_review_session().await;
        let params = test_review_params().await;
        let deadline = params.deadline;
        child.reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            &params.spawn_config,
            params.parent_session.user_instructions().await,
        );
        let (release_shutdown, shutdown_gate) = tokio::sync::oneshot::channel();
        child.codex.session_loop_termination =
            crate::session::session_loop_termination_from_handle(tokio::spawn(async move {
                shutdown_gate.await.expect("release child shutdown");
            }));
        let child_stopped = child.codex.session_loop_termination.clone();
        let cancelled = child.cancel_token.clone();
        let manager = Arc::new(GuardianReviewSessionManager {
            state: Arc::new(Mutex::new(GuardianReviewSessionState {
                trunk: Some(Arc::new(child)),
                ephemeral_reviews: Vec::new(),
            })),
            ..Default::default()
        });
        let runner = Arc::clone(&manager);
        let review = tokio::spawn(async move { runner.run_review(params).await });
        let submitted = tokio::time::timeout(Duration::from_secs(5), rx_sub.recv())
            .await
            .expect("normal review submits to child")
            .expect("review submission");
        assert!(matches!(submitted.op, Op::UserInput { .. }));
        tx_event
            .send(turn_complete_event("prior-turn", Some("stale"), Some(7)))
            .await
            .expect("queue stale completion");
        // Hold only the retirement registry after admission and the normal submission.
        let registry = manager.state.lock().await;
        tokio::time::pause();
        tokio::time::advance(deadline.saturating_duration_since(tokio::time::Instant::now())).await;
        let (outcome, analytics) = tokio::time::timeout(Duration::from_millis(1), review)
            .await
            .expect("absolute review deadline must not await retirement")
            .expect("review task");
        assert!(matches!(outcome, GuardianReviewSessionOutcome::TimedOut));
        assert!(analytics.guardian_thread_id.is_some());
        assert!(
            cancelled.is_cancelled(),
            "retiring child is not reusable after result"
        );
        assert!(
            rx_sub.try_recv().is_err(),
            "no interrupt or shutdown can bypass held registry"
        );
        drop(registry);
        let shutdown = tokio::time::timeout(Duration::from_secs(1), rx_sub.recv())
            .await
            .expect("owned cleanup resumes after registry release")
            .expect("shutdown submission");
        assert!(matches!(shutdown.op, Op::Shutdown));
        assert!(manager.state.lock().await.trunk.is_none());
        // Normal manager shutdown still owns the externally blocked child termination.
        let shutdown_manager = Arc::clone(&manager);
        let mut stop = tokio::spawn(async move { shutdown_manager.shutdown().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut stop)
                .await
                .is_err()
        );
        release_shutdown
            .send(())
            .expect("allow child loop to finish");
        tokio::time::timeout(Duration::from_secs(1), child_stopped)
            .await
            .expect("actual child loop completes");
        tokio::time::timeout(Duration::from_secs(1), stop)
            .await
            .expect("manager awaits actual completion")
            .expect("shutdown task");
        assert!(
            rx_sub.try_recv().is_err(),
            "child shutdown is submitted once"
        );
        tokio::time::resume();
    }

    #[tokio::test]
    async fn guardian_shutdown_grace_retains_cleanup_when_admitted_review_future_is_dropped() {
        let (mut child, _tx_event, rx_sub) = test_review_session().await;
        let params = test_review_params().await;
        let rejected_params = test_review_params().await;
        child.reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            &params.spawn_config,
            params.parent_session.user_instructions().await,
        );
        let (release_shutdown, shutdown_gate) = tokio::sync::oneshot::channel();
        child.codex.session_loop_termination =
            crate::session::session_loop_termination_from_handle(tokio::spawn(async move {
                shutdown_gate.await.expect("release owned child shutdown");
            }));
        let child_stopped = child.codex.session_loop_termination.clone();
        let manager = Arc::new(GuardianReviewSessionManager {
            state: Arc::new(Mutex::new(GuardianReviewSessionState {
                trunk: Some(Arc::new(child)),
                ephemeral_reviews: Vec::new(),
            })),
            ..Default::default()
        });
        let runner = Arc::clone(&manager);
        let review = tokio::spawn(async move { runner.run_review(params).await });
        let submitted = tokio::time::timeout(Duration::from_secs(5), rx_sub.recv())
            .await
            .expect("normal review submits to child")
            .expect("review submission");
        assert!(matches!(submitted.op, Op::UserInput { .. }));
        let registry = manager.state.lock().await;
        tokio::time::pause();
        let mut shutdown = Box::pin(manager.shutdown());
        assert!(futures::poll!(shutdown.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(1_999)).await;
        assert!(
            futures::poll!(shutdown.as_mut()).is_pending(),
            "shutdown must retain the two-second grace while admitted cleanup is blocked"
        );
        // Tokio rounds timer deadlines up to the next millisecond. The clock was
        // paused after session setup, so it need not align with the timer epoch.
        tokio::time::advance(Duration::from_millis(2)).await;
        assert!(
            futures::poll!(shutdown.as_mut()).is_ready(),
            "the caller must return within one timer tick of the two-second grace"
        );
        drop(shutdown);
        let (outcome, analytics) = tokio::time::timeout(
            Duration::from_millis(1),
            manager.run_review(rejected_params),
        )
        .await
        .expect("closed manager rejects admission without waiting for registry");
        assert!(matches!(outcome, GuardianReviewSessionOutcome::Aborted));
        assert!(analytics.guardian_thread_id.is_none());
        review.abort();
        assert!(
            review
                .await
                .expect_err("caller future is dropped")
                .is_cancelled()
        );
        assert!(
            rx_sub.try_recv().is_err(),
            "no new review or early shutdown while registry is held"
        );
        // Cleanup admitted before close must survive both graceful timeout and caller drop.
        drop(registry);
        let shutdown = tokio::time::timeout(Duration::from_secs(1), rx_sub.recv())
            .await
            .expect("retained cleanup runs after lock release")
            .expect("shutdown submission");
        assert!(matches!(shutdown.op, Op::Shutdown));
        assert!(manager.state.lock().await.trunk.is_none());
        assert!(
            tokio::time::timeout(
                Duration::from_millis(1),
                manager.background_shutdowns.wait()
            )
            .await
            .is_err(),
            "closed tracker cannot claim completion while actual child is still stopping"
        );
        release_shutdown
            .send(())
            .expect("release actual child loop");
        tokio::time::timeout(Duration::from_secs(1), child_stopped)
            .await
            .expect("actual child stops");
        tokio::time::timeout(Duration::from_secs(1), manager.background_shutdowns.wait())
            .await
            .expect("all admitted cleanup eventually completes");
        assert!(
            rx_sub.try_recv().is_err(),
            "drain and dropped review must not shut down child twice"
        );
        tokio::time::resume();
    }
}
