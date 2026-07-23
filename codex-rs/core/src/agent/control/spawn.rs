use super::residency::is_v2_resident_session_source;
use super::*;
use codex_extension_api::ExtensionDataInit;
use tracing::Instrument;

const AGENT_NAMES: &str = include_str!("../agent_names.txt");

struct SpawnAgentThreadInheritance {
    environments: Option<TurnEnvironmentSnapshot>,
    exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
}

/// Initial input delivered after a spawned agent acquires execution capacity.
///
/// V2 communication spawns keep the communication and its context paired so centralized
/// submission and lifecycle logging cannot receive one without the other. Other spawn sources
/// provide user input directly, making an uncontextualized inter-agent communication
/// unrepresentable.
enum SpawnInitialInput {
    UserInput(Vec<UserInput>),
    InterAgentCommunication(InterAgentCommunication, AgentCommunicationContext),
}

struct PendingSpawnCleanup {
    control: AgentControl,
    child_thread: Arc<crate::CodexThread>,
    child_thread_id: ThreadId,
    armed: bool,
}

impl PendingSpawnCleanup {
    fn new(
        control: AgentControl,
        child_thread: Arc<crate::CodexThread>,
        child_thread_id: ThreadId,
    ) -> Self {
        Self {
            control,
            child_thread,
            child_thread_id,
            armed: true,
        }
    }

    async fn rollback(mut self, submission_error: CodexErr) -> CodexErr {
        let error = self
            .control
            .rollback_failed_initial_submission(
                self.child_thread.as_ref(),
                self.child_thread_id,
                submission_error,
            )
            .await;
        self.armed = false;
        error
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingSpawnCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let control = self.control.clone();
        let child_thread = Arc::clone(&self.child_thread);
        let child_thread_id = self.child_thread_id;
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            warn!(
                %child_thread_id,
                "unable to schedule cleanup for cancelled agent spawn without a Tokio runtime"
            );
            return;
        };
        drop(runtime.spawn(async move {
            let _ = control
                .rollback_failed_initial_submission(
                    child_thread.as_ref(),
                    child_thread_id,
                    CodexErr::TurnAborted,
                )
                .await;
        }));
    }
}

struct V2AgentLoadOwner {
    thread_id: ThreadId,
    completion: Arc<watch::Sender<V2AgentLoadCompletion>>,
    flights: Arc<std::sync::Mutex<HashMap<ThreadId, Arc<watch::Sender<V2AgentLoadCompletion>>>>>,
    completed: bool,
}

impl V2AgentLoadOwner {
    fn finish(mut self, result: &CodexResult<()>) {
        let completion = match result {
            Ok(()) => V2AgentLoadCompletion::Succeeded,
            Err(error) => V2AgentLoadCompletion::Failed(Arc::from(error.to_string())),
        };
        self.complete(completion);
    }

    fn complete(&mut self, completion: V2AgentLoadCompletion) {
        let _ = self.completion.send_replace(completion);
        let mut flights = self
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if flights
            .get(&self.thread_id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.completion))
        {
            flights.remove(&self.thread_id);
        }
        self.completed = true;
    }
}

impl Drop for V2AgentLoadOwner {
    fn drop(&mut self) {
        if !self.completed {
            self.complete(V2AgentLoadCompletion::Cancelled);
        }
    }
}

enum V2AgentLoadFlight {
    Owner(V2AgentLoadOwner),
    Follower(watch::Receiver<V2AgentLoadCompletion>),
}

fn default_agent_nickname_list() -> Vec<&'static str> {
    AGENT_NAMES
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

pub(super) fn agent_nickname_candidates(config: &Config, role_name: Option<&str>) -> Vec<String> {
    let role_name = role_name.unwrap_or(DEFAULT_ROLE_NAME);
    if let Some(candidates) =
        resolve_role_config(config, role_name).and_then(|role| role.nickname_candidates.clone())
    {
        return candidates;
    }

    default_agent_nickname_list()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
}

fn keep_forked_rollout_item(item: &RolloutItem, preserve_reference_context_item: bool) -> bool {
    match item {
        RolloutItem::ResponseItem(ResponseItem::Message { role, phase, .. }) => match role.as_str()
        {
            "system" | "developer" | "user" => true,
            "assistant" => *phase == Some(MessagePhase::FinalAnswer),
            _ => false,
        },
        RolloutItem::ResponseItem(
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other,
        ) => false,
        RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. } => false,
        // Full-history forks preserve the cached prompt prefix and can keep diffing
        // from the parent's durable baseline. Truncated forks drop part of that prompt,
        // so they must rebuild context on their first child turn.
        RolloutItem::TurnContext(_) | RolloutItem::WorldState(_) => preserve_reference_context_item,
        RolloutItem::Compacted(_) | RolloutItem::EventMsg(_) | RolloutItem::SessionMeta(_) => true,
    }
}

fn is_multi_agent_v2_usage_hint_message(item: &ResponseItem, usage_hint_texts: &[String]) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };
    if role != "developer" {
        return false;
    }
    let [ContentItem::InputText { text }] = content.as_slice() else {
        return false;
    };

    usage_hint_texts
        .iter()
        .any(|usage_hint_text| usage_hint_text == text)
}

impl AgentControl {
    /// Spawn a new agent thread and submit the initial prompt.
    #[cfg(test)]
    pub(crate) async fn spawn_agent(
        &self,
        config: Config,
        initial_input: Vec<UserInput>,
        session_source: Option<SessionSource>,
    ) -> CodexResult<ThreadId> {
        let spawned_agent = Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::UserInput(initial_input),
            session_source,
            SpawnAgentOptions::default(),
        ))
        .await?;
        Ok(spawned_agent.thread_id)
    }

    /// Spawn an agent thread with some metadata.
    pub(crate) async fn spawn_agent_with_metadata(
        &self,
        config: Config,
        initial_input: Vec<UserInput>,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions, // TODO(jif) drop with new fork.
    ) -> CodexResult<LiveAgent> {
        Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::UserInput(initial_input),
            session_source,
            options,
        ))
        .await
    }

    pub(crate) async fn spawn_agent_with_communication(
        &self,
        config: Config,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<LiveAgent> {
        Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::InterAgentCommunication(communication, context),
            session_source,
            options,
        ))
        .await
    }

    pub(crate) async fn ensure_v2_agent_loaded(
        &self,
        config: Config,
        thread_id: ThreadId,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        if self
            .use_loaded_v2_agent_or_clear_stopped(&state, thread_id)
            .await?
        {
            return Ok(());
        }
        if self.state.agent_metadata_for_thread(thread_id).is_none() {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }

        loop {
            match self.begin_v2_agent_load(thread_id) {
                V2AgentLoadFlight::Owner(load_owner) => {
                    let control = self.clone();
                    let load_state = Arc::clone(&state);
                    // The keyed load owns its lifecycle independently of any one waiter. If the
                    // caller is cancelled after the runtime enters the manager, the load must
                    // still commit residency and publish completion instead of leaking an
                    // uncharged runtime.
                    let load_task = tokio::spawn(
                        async move {
                            let result = control
                                .load_v2_agent_as_owner(&load_state, config, thread_id)
                                .await;
                            load_owner.finish(&result);
                            result
                        }
                        .in_current_span(),
                    );
                    return match load_task.await {
                        Ok(result) => result,
                        Err(error) => Err(CodexErr::Fatal(format!(
                            "V2 agent load task failed for {thread_id}: {error}"
                        ))),
                    };
                }
                V2AgentLoadFlight::Follower(completion) => {
                    let completion = Self::wait_for_v2_agent_load(completion).await;
                    if self
                        .use_loaded_v2_agent_or_clear_stopped(&state, thread_id)
                        .await?
                    {
                        return Ok(());
                    }
                    if self.state.agent_metadata_for_thread(thread_id).is_none() {
                        return Err(CodexErr::ThreadNotFound(thread_id));
                    }
                    if let V2AgentLoadCompletion::Failed(error) = completion {
                        return Err(CodexErr::Fatal(format!(
                            "failed to load V2 agent {thread_id}: {error}"
                        )));
                    }
                }
            }
        }
    }

    fn begin_v2_agent_load(&self, thread_id: ThreadId) -> V2AgentLoadFlight {
        let mut flights = self
            .v2_load_flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(completion) = flights.get(&thread_id) {
            return V2AgentLoadFlight::Follower(completion.subscribe());
        }

        let (completion, _) = watch::channel(V2AgentLoadCompletion::Loading);
        let completion = Arc::new(completion);
        flights.insert(thread_id, Arc::clone(&completion));
        V2AgentLoadFlight::Owner(V2AgentLoadOwner {
            thread_id,
            completion,
            flights: Arc::clone(&self.v2_load_flights),
            completed: false,
        })
    }

    async fn wait_for_v2_agent_load(
        mut completion: watch::Receiver<V2AgentLoadCompletion>,
    ) -> V2AgentLoadCompletion {
        loop {
            let current = completion.borrow_and_update().clone();
            if !matches!(current, V2AgentLoadCompletion::Loading) {
                return current;
            }
            if completion.changed().await.is_err() {
                return V2AgentLoadCompletion::Cancelled;
            }
        }
    }

    pub(super) async fn use_loaded_v2_agent_or_clear_stopped(
        &self,
        state: &Arc<ThreadManagerState>,
        thread_id: ThreadId,
    ) -> CodexResult<bool> {
        loop {
            let thread = match state.get_thread(thread_id).await {
                Ok(thread) => thread,
                Err(CodexErr::ThreadNotFound(_)) => {
                    self.forget_v2_residency(thread_id);
                    match state.get_thread(thread_id).await {
                        Ok(_) => continue,
                        Err(CodexErr::ThreadNotFound(_)) => return Ok(false),
                        Err(err) => return Err(err),
                    }
                }
                Err(err) => return Err(err),
            };
            if !thread.is_running() && state.remove_thread_if_same(&thread_id, &thread).await {
                self.forget_v2_residency(thread_id);
                match state.get_thread(thread_id).await {
                    Ok(_) => continue,
                    Err(CodexErr::ThreadNotFound(_)) => return Ok(false),
                    Err(err) => return Err(err),
                }
            }
            if !thread.is_running() {
                continue;
            }
            if !self.touch_loaded_v2_residency(state, thread_id).await {
                continue;
            }
            if !thread.is_running() {
                if state.remove_thread_if_same(&thread_id, &thread).await {
                    self.forget_v2_residency(thread_id);
                    match state.get_thread(thread_id).await {
                        Ok(_) => continue,
                        Err(CodexErr::ThreadNotFound(_)) => return Ok(false),
                        Err(err) => return Err(err),
                    }
                }
                continue;
            }
            match state.get_thread(thread_id).await {
                Ok(current) if Arc::ptr_eq(&current, &thread) && thread.is_running() => {
                    return Ok(true);
                }
                Ok(_) => continue,
                Err(CodexErr::ThreadNotFound(_)) => {
                    self.forget_v2_residency(thread_id);
                    match state.get_thread(thread_id).await {
                        Ok(_) => continue,
                        Err(CodexErr::ThreadNotFound(_)) => return Ok(false),
                        Err(err) => return Err(err),
                    }
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn load_v2_agent_as_owner(
        &self,
        state: &Arc<ThreadManagerState>,
        config: Config,
        thread_id: ThreadId,
    ) -> CodexResult<()> {
        if self
            .use_loaded_v2_agent_or_clear_stopped(state, thread_id)
            .await?
        {
            return Ok(());
        }
        if self.state.agent_metadata_for_thread(thread_id).is_none() {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }

        #[cfg(test)]
        self.pause_before_v2_cold_load_for_test().await;

        let stored_thread = state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: true,
            })
            .await?;
        let stored_source = stored_thread.source.clone();
        let stored_parent_thread_id = stored_thread.parent_thread_id;
        let history = stored_thread
            .history
            .ok_or(CodexErr::ThreadNotFound(thread_id))?
            .items;
        let initial_history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(history),
            rollout_path: stored_thread.rollout_path,
        });
        if initial_history.get_multi_agent_version() != Some(MultiAgentVersion::V2) {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        let residency_slot = self
            .reserve_v2_residency_slot(state, &config, Some(thread_id))
            .await?;

        let (session_source, _) = initial_history
            .get_resumed_session_sources()
            .unwrap_or((stored_source, None));
        let parent_thread_id = initial_history
            .get_resumed_parent_thread_id()
            .or(stored_parent_thread_id);
        let inherited_environments = self
            .inherited_environments_for_source(state, Some(&session_source))
            .await;
        let inherited_exec_policy = self
            .inherited_exec_policy_for_source(state, Some(&session_source), &config)
            .await;

        match state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                config,
                initial_history,
                agent_control: self.clone(),
                session_source,
                parent_thread_id,
                inherited_environments,
                inherited_exec_policy,
            })
            .await
        {
            Ok(reloaded_thread) => {
                residency_slot.commit(reloaded_thread.thread_id);
                state.notify_thread_created(reloaded_thread.thread_id);
                Ok(())
            }
            Err(err) => {
                drop(residency_slot);
                if self
                    .use_loaded_v2_agent_or_clear_stopped(state, thread_id)
                    .await?
                {
                    return Ok(());
                }
                Err(err)
            }
        }
    }

    #[cfg(test)]
    async fn pause_before_initial_submission_for_test(&self) {
        let barrier = self
            .test_hooks
            .before_initial_submission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(barrier) = barrier {
            barrier.pause().await;
        }
    }

    #[cfg(test)]
    async fn pause_before_v2_cold_load_for_test(&self) {
        let barrier = self
            .test_hooks
            .before_v2_cold_load
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(barrier) = barrier {
            barrier.pause().await;
        }
    }

    async fn spawn_agent_internal(
        &self,
        config: Config,
        initial_input: SpawnInitialInput,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<LiveAgent> {
        let state = self.upgrade()?;
        let multi_agent_version = state
            .effective_multi_agent_version_for_spawn(
                &InitialHistory::New,
                session_source.as_ref(),
                options.parent_thread_id,
                /*forked_from_thread_id*/ None,
                &config,
            )
            .await;
        let execution_guard = if let Some(session_source) = session_source.as_ref() {
            self.reserve_execution_capacity(multi_agent_version, session_source)?
        } else {
            None
        };
        let agent_max_threads = config.effective_agent_max_threads(multi_agent_version);
        let spawn_uses_v2_residency = multi_agent_version == MultiAgentVersion::V2
            && session_source
                .as_ref()
                .is_some_and(is_v2_resident_session_source);
        let reservation_max_threads = if spawn_uses_v2_residency {
            None
        } else {
            agent_max_threads
        };
        let mut reservation = self.state.reserve_spawn_slot(reservation_max_threads)?;
        let (session_source, mut agent_metadata) = match session_source {
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_role,
                ..
            })) => {
                let (session_source, agent_metadata) = self.prepare_thread_spawn(
                    &mut reservation,
                    &config,
                    parent_thread_id,
                    depth,
                    agent_path,
                    agent_role,
                    /*preferred_agent_nickname*/ None,
                )?;
                (Some(session_source), agent_metadata)
            }
            other => (other, AgentMetadata::default()),
        };
        let notification_source = session_source.clone();
        let residency_slot = if spawn_uses_v2_residency {
            Some(
                self.reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
                    .await?,
            )
        } else {
            None
        };
        let inheritance = SpawnAgentThreadInheritance {
            environments: self
                .inherited_environments_for_source(&state, session_source.as_ref())
                .await,
            exec_policy: self
                .inherited_exec_policy_for_source(&state, session_source.as_ref(), &config)
                .await,
        };

        // The same `AgentControl` is sent to spawn the thread.
        let new_thread = match (session_source, options.fork_mode.as_ref(), inheritance) {
            (Some(session_source), Some(_), inheritance) => {
                Box::pin(self.spawn_forked_thread(
                    &state,
                    config,
                    session_source,
                    &options,
                    inheritance,
                    multi_agent_version,
                ))
                .await?
            }
            (Some(session_source), None, inheritance) => {
                Box::pin(state.spawn_new_thread_with_source(
                    config.clone(),
                    self.clone(),
                    session_source,
                    options.parent_thread_id,
                    /*forked_from_thread_id*/ None,
                    /*thread_source*/ Some(ThreadSource::Subagent),
                    /*metrics_service_name*/ None,
                    inheritance.environments,
                    inheritance.exec_policy,
                    options.environments.clone(),
                ))
                .await?
            }
            (None, _, _) => Box::pin(state.spawn_new_thread(config.clone(), self.clone())).await?,
        };
        let mut pending_cleanup = PendingSpawnCleanup::new(
            self.clone(),
            Arc::clone(&new_thread.thread),
            new_thread.thread_id,
        );
        agent_metadata.agent_id = Some(new_thread.thread_id);

        self.persist_thread_spawn_edge_for_source(
            new_thread.thread.as_ref(),
            new_thread.thread_id,
            notification_source.as_ref(),
        )
        .await;

        if let Some(mut binding) = options.typed_task_binding.clone() {
            let spawned_agent_path = agent_metadata.agent_path.as_ref().map(ToString::to_string);
            if spawned_agent_path.as_deref() != Some(binding.agent_path.as_str()) {
                return Err(pending_cleanup
                    .rollback(CodexErr::Fatal(format!(
                        "typed task binding path {} does not match spawned agent path {}",
                        binding.agent_path,
                        spawned_agent_path.as_deref().unwrap_or("<missing>")
                    )))
                    .await);
            }
            binding.thread_id = Some(new_thread.thread_id.to_string());
            if let Err(error) = self.task_coordinator().bind_agent_task(binding).await {
                return Err(pending_cleanup
                    .rollback(CodexErr::Fatal(format!(
                        "failed to bind typed task before starting spawned agent: {error}"
                    )))
                    .await);
            }
        }

        if let Some(binding) = options.agent_job_binding.as_ref() {
            let spawned_thread_id = new_thread.thread_id.to_string();
            let bound = binding
                .state_db
                .mark_agent_job_item_running_with_thread(
                    binding.job_id.as_str(),
                    binding.item_id.as_str(),
                    spawned_thread_id.as_str(),
                )
                .await;
            match bound {
                Ok(true) => {}
                Ok(false) => {
                    return Err(pending_cleanup
                        .rollback(CodexErr::Fatal(format!(
                            "agent job item {}/{} is no longer claimable",
                            binding.job_id, binding.item_id
                        )))
                        .await);
                }
                Err(error) => {
                    return Err(pending_cleanup
                        .rollback(CodexErr::Fatal(format!(
                                "failed to bind agent job item {}/{} before starting spawned agent: {error}",
                                binding.job_id, binding.item_id
                            )))
                        .await);
                }
            }
        }

        let initial_last_task_message = match &initial_input {
            SpawnInitialInput::UserInput(input) => {
                non_empty_task_message(render_input_preview(input))
            }
            SpawnInitialInput::InterAgentCommunication(communication, _) => {
                last_task_message_from_communication(communication)
            }
        };
        #[cfg(test)]
        self.pause_before_initial_submission_for_test().await;
        let initial_submission_result = match initial_input {
            SpawnInitialInput::UserInput(input) => {
                self.send_input_after_capacity_check(
                    new_thread.thread_id,
                    &state,
                    input,
                    execution_guard,
                )
                .await
            }
            SpawnInitialInput::InterAgentCommunication(communication, context) => {
                self.send_inter_agent_communication_after_capacity_check(
                    new_thread.thread_id,
                    &state,
                    communication,
                    context,
                    execution_guard,
                )
                .await
            }
        };
        if let Err(err) = initial_submission_result {
            return Err(pending_cleanup.rollback(err).await);
        }
        agent_metadata.last_task_message = initial_last_task_message;
        if let Err(err) = reservation.commit(agent_metadata.clone()) {
            return Err(pending_cleanup.rollback(err).await);
        }
        if let Some(residency_slot) = residency_slot {
            residency_slot.commit(new_thread.thread_id);
        }

        // Notify only after the initial work is queued and the child becomes path-addressable.
        state.notify_thread_created(new_thread.thread_id);
        pending_cleanup.disarm();

        if let Some(SessionSource::SubAgent(
            subagent_source @ SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            },
        )) = notification_source.as_ref()
        {
            let client_metadata = match state.get_thread(*parent_thread_id).await {
                Ok(parent_thread) => {
                    parent_thread
                        .codex
                        .session
                        .app_server_client_metadata()
                        .await
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        parent_thread_id = %parent_thread_id,
                        "skipping subagent thread analytics: failed to load parent thread metadata"
                    );
                    crate::session::session::AppServerClientMetadata {
                        client_name: None,
                        client_version: None,
                    }
                }
            };
            let thread_config = new_thread.thread.codex.thread_config_snapshot().await;
            let parent_thread_id = thread_config.parent_thread_id;
            emit_subagent_session_started(
                &new_thread
                    .thread
                    .codex
                    .session
                    .services
                    .analytics_events_client,
                client_metadata,
                new_thread.thread.codex.session.session_id(),
                new_thread.thread_id,
                parent_thread_id,
                thread_config,
                subagent_source.clone(),
            );
        }
        if multi_agent_version != MultiAgentVersion::V2 {
            let child_reference = agent_metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| new_thread.thread_id.to_string());
            self.maybe_start_completion_watcher(
                new_thread.thread_id,
                notification_source,
                child_reference,
                agent_metadata.agent_path.clone(),
            );
        }

        Ok(LiveAgent {
            thread_id: new_thread.thread_id,
            metadata: agent_metadata,
            status: self.get_status(new_thread.thread_id).await,
        })
    }

    pub(crate) async fn rollback_failed_initial_submission(
        &self,
        child_thread: &crate::CodexThread,
        child_thread_id: ThreadId,
        submission_error: CodexErr,
    ) -> CodexErr {
        let mut cleanup_failures = Vec::new();
        match self.upgrade() {
            Ok(state) => {
                if !child_thread.config_snapshot().await.ephemeral
                    && let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            child_thread_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    cleanup_failures
                        .push(format!("failed to persist closed spawn-edge status: {err}"));
                }
            }
            Err(err) => cleanup_failures.push(format!(
                "failed to access thread manager for spawn rollback: {err}"
            )),
        }

        match Box::pin(self.shutdown_agent_tree(child_thread_id)).await {
            Ok(_) | Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied) => {}
            Err(err) => {
                cleanup_failures.push(format!("failed to shut down spawned subtree: {err}"))
            }
        }

        if cleanup_failures.is_empty() {
            submission_error
        } else {
            CodexErr::Fatal(format!(
                "initial submission to spawned agent {child_thread_id} failed ({submission_error}); cleanup was incomplete: {}",
                cleanup_failures.join("; ")
            ))
        }
    }

    async fn spawn_forked_thread(
        &self,
        state: &Arc<ThreadManagerState>,
        config: Config,
        session_source: SessionSource,
        options: &SpawnAgentOptions,
        inheritance: SpawnAgentThreadInheritance,
        multi_agent_version: MultiAgentVersion,
    ) -> CodexResult<crate::thread_manager::NewThread> {
        let SpawnAgentThreadInheritance {
            environments: inherited_environments,
            exec_policy: inherited_exec_policy,
        } = inheritance;
        if options.fork_parent_spawn_call_id.is_none() {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a parent spawn call id".to_string(),
            ));
        }
        let Some(fork_mode) = options.fork_mode.as_ref() else {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a fork mode".to_string(),
            ));
        };
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = &session_source
        else {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a thread-spawn session source".to_string(),
            ));
        };

        let parent_thread_id = *parent_thread_id;
        let parent_thread = state.get_thread(parent_thread_id).await.ok();
        if let Some(parent_thread) = parent_thread.as_ref() {
            // `record_conversation_items` only queues persistence writes asynchronously.
            // Flush before snapshotting store history for a fork.
            parent_thread.ensure_rollout_materialized().await;
            parent_thread.flush_rollout().await?;
        }

        let parent_history = state
            .read_stored_thread(ReadThreadParams {
                thread_id: parent_thread_id,
                include_archived: true,
                include_history: true,
            })
            .await?
            .history
            .ok_or_else(|| {
                CodexErr::Fatal(format!(
                    "parent thread history unavailable for fork: {parent_thread_id}"
                ))
            })?;

        let selected_capability_roots = parent_history
            .items
            .iter()
            .find_map(|item| {
                let RolloutItem::SessionMeta(meta_line) = item else {
                    return None;
                };
                Some(meta_line.meta.selected_capability_roots.clone())
            })
            .unwrap_or_default();
        let mut forked_rollout_items = parent_history.items;
        if let SpawnAgentForkMode::LastNTurns(last_n_turns) = fork_mode {
            forked_rollout_items =
                truncate_rollout_to_last_n_fork_turns(&forked_rollout_items, *last_n_turns);
        }
        let multi_agent_v2_usage_hint_texts_to_filter: Vec<String> =
            if let Some(parent_thread) = parent_thread.as_ref() {
                if multi_agent_version == MultiAgentVersion::V2 {
                    let parent_config = parent_thread.codex.session.get_config().await;
                    [
                        parent_config
                            .multi_agent_v2
                            .root_agent_usage_hint_text
                            .clone(),
                        parent_config
                            .multi_agent_v2
                            .subagent_usage_hint_text
                            .clone(),
                    ]
                    .into_iter()
                    .flatten()
                    .collect()
                } else {
                    Vec::new()
                }
            } else if multi_agent_version == MultiAgentVersion::V2 {
                [
                    config.multi_agent_v2.root_agent_usage_hint_text.clone(),
                    config.multi_agent_v2.subagent_usage_hint_text.clone(),
                ]
                .into_iter()
                .flatten()
                .collect()
            } else {
                Vec::new()
            };
        let preserve_reference_context_item = matches!(fork_mode, SpawnAgentForkMode::FullHistory);
        forked_rollout_items.retain(|item| {
            keep_forked_rollout_item(item, preserve_reference_context_item)
                && !matches!(
                    item,
                    RolloutItem::ResponseItem(response_item)
                        if is_multi_agent_v2_usage_hint_message(
                            response_item,
                            &multi_agent_v2_usage_hint_texts_to_filter,
                        )
                )
        });
        for item in &mut forked_rollout_items {
            if let RolloutItem::Compacted(compacted) = item
                && let Some(replacement_history) = compacted.replacement_history.as_mut()
            {
                replacement_history.retain(|response_item| {
                    !is_multi_agent_v2_usage_hint_message(
                        response_item,
                        &multi_agent_v2_usage_hint_texts_to_filter,
                    )
                });
            }
        }
        if preserve_reference_context_item
            && multi_agent_version == MultiAgentVersion::V2
            && let Some(subagent_usage_hint_text) =
                config.multi_agent_v2.subagent_usage_hint_text.clone()
            && let Some(subagent_usage_hint_message) =
                crate::context_manager::updates::build_developer_update_item(vec![
                    subagent_usage_hint_text,
                ])
        {
            forked_rollout_items.push(RolloutItem::ResponseItem(subagent_usage_hint_message));
        }
        let mut thread_extension_init = ExtensionDataInit::new();
        thread_extension_init.insert(selected_capability_roots);

        state
            .fork_thread_with_source(
                config.clone(),
                InitialHistory::Forked(forked_rollout_items),
                self.clone(),
                session_source,
                /*thread_source*/ Some(ThreadSource::Subagent),
                /*parent_thread_id*/ Some(parent_thread_id),
                /*forked_from_thread_id*/ Some(parent_thread_id),
                inherited_environments,
                inherited_exec_policy,
                options.environments.clone(),
                thread_extension_init,
            )
            .await
    }

    /// Resume an existing agent thread from a recorded rollout file.
    pub(crate) async fn resume_agent_from_rollout(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<ThreadId> {
        let root_depth = thread_spawn_depth(&session_source).unwrap_or(0);
        let (resumed_thread_id, resumed_multi_agent_version) = Box::pin(
            self.resume_single_agent_from_rollout(config.clone(), thread_id, session_source),
        )
        .await?;
        let state = self.upgrade()?;
        if config.multi_agent_version_from_features() == MultiAgentVersion::V2
            || resumed_multi_agent_version == MultiAgentVersion::V2
        {
            return Ok(resumed_thread_id);
        }
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(resumed_thread_id);
        };

        let mut resume_queue = VecDeque::from([(thread_id, root_depth)]);
        let mut seen_thread_ids = HashSet::from([thread_id]);
        while let Some((parent_thread_id, parent_depth)) = resume_queue.pop_front() {
            let child_ids = match agent_graph_store
                .list_thread_spawn_children(
                    parent_thread_id,
                    Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
                )
                .await
            {
                Ok(child_ids) => child_ids,
                Err(err) => {
                    warn!(
                        "failed to load persisted thread-spawn children for {parent_thread_id}: {err}"
                    );
                    continue;
                }
            };

            for child_thread_id in child_ids {
                if !seen_thread_ids.insert(child_thread_id) {
                    warn!(
                        "skipping repeated persisted thread-spawn edge to {child_thread_id} while restoring {thread_id}"
                    );
                    continue;
                }
                let child_depth = parent_depth + 1;
                let child_resumed = if state.get_thread(child_thread_id).await.is_ok() {
                    true
                } else {
                    let child_session_source =
                        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id,
                            depth: child_depth,
                            agent_path: None,
                            agent_nickname: None,
                            agent_role: None,
                        });
                    match Box::pin(self.resume_single_agent_from_rollout(
                        config.clone(),
                        child_thread_id,
                        child_session_source,
                    ))
                    .await
                    {
                        Ok((_, _)) => true,
                        Err(err) => {
                            if matches!(&err, CodexErr::ThreadNotFound(_))
                                && let Err(close_err) = agent_graph_store
                                    .set_thread_spawn_edge_status(
                                        child_thread_id,
                                        codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                                    )
                                    .await
                            {
                                warn!(
                                    "failed to close unrecoverable persisted thread-spawn edge for {child_thread_id}: {close_err}"
                                );
                            }
                            warn!("failed to resume descendant thread {child_thread_id}: {err}");
                            false
                        }
                    }
                };
                if child_resumed {
                    resume_queue.push_back((child_thread_id, child_depth));
                }
            }
        }

        Ok(resumed_thread_id)
    }

    async fn resume_single_agent_from_rollout(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<(ThreadId, MultiAgentVersion)> {
        let state = self.upgrade()?;
        let stored_thread = state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: true,
            })
            .await?;
        let resumed_agent_path = stored_thread
            .agent_path
            .as_deref()
            .map(AgentPath::try_from)
            .transpose()
            .map_err(|err| CodexErr::InvalidRequest(format!("invalid stored agent path: {err}")))?;
        let resumed_agent_nickname = stored_thread.agent_nickname.clone();
        let resumed_agent_role = stored_thread.agent_role.clone();
        let history = stored_thread
            .history
            .ok_or_else(|| CodexErr::ThreadNotFound(thread_id))?
            .items;
        let initial_history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(history),
            rollout_path: stored_thread.rollout_path,
        });
        let parent_thread_id = stored_thread.parent_thread_id;
        let multi_agent_version = state
            .effective_multi_agent_version_for_spawn(
                &initial_history,
                Some(&session_source),
                parent_thread_id,
                /*forked_from_thread_id*/ None,
                &config,
            )
            .await;
        let agent_max_threads = config.effective_agent_max_threads(multi_agent_version);
        let mut reservation = self.state.reserve_spawn_slot(agent_max_threads)?;
        let (session_source, agent_metadata) = match session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_role: _,
                agent_nickname: _,
            }) => self.prepare_thread_spawn(
                &mut reservation,
                &config,
                parent_thread_id,
                depth,
                agent_path.or(resumed_agent_path),
                resumed_agent_role,
                resumed_agent_nickname,
            )?,
            other => (other, AgentMetadata::default()),
        };
        let notification_source = session_source.clone();
        let inherited_environments = self
            .inherited_environments_for_source(&state, Some(&session_source))
            .await;
        let inherited_exec_policy = self
            .inherited_exec_policy_for_source(&state, Some(&session_source), &config)
            .await;

        let resumed_thread = state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                config: config.clone(),
                initial_history,
                agent_control: self.clone(),
                session_source,
                parent_thread_id,
                inherited_environments,
                inherited_exec_policy,
            })
            .await?;
        let mut agent_metadata = agent_metadata;
        agent_metadata.agent_id = Some(resumed_thread.thread_id);
        if let Err(err) = reservation.commit(agent_metadata.clone()) {
            let shutdown_result = self.shutdown_live_agent(resumed_thread.thread_id).await;
            return match shutdown_result {
                Ok(_) | Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied) => {
                    Err(err)
                }
                Err(shutdown_err) => Err(CodexErr::Fatal(format!(
                    "failed to register resumed agent {} ({err}); cleanup failed: {shutdown_err}",
                    resumed_thread.thread_id
                ))),
            };
        }
        // Resumed threads are re-registered in-memory and need the same listener
        // attachment path as freshly spawned threads.
        state.notify_thread_created(resumed_thread.thread_id);
        if multi_agent_version != MultiAgentVersion::V2 {
            let child_reference = agent_metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| resumed_thread.thread_id.to_string());
            self.maybe_start_completion_watcher(
                resumed_thread.thread_id,
                Some(notification_source.clone()),
                child_reference,
                agent_metadata.agent_path.clone(),
            );
        }
        self.persist_thread_spawn_edge_for_source(
            resumed_thread.thread.as_ref(),
            resumed_thread.thread_id,
            Some(&notification_source),
        )
        .await;

        Ok((resumed_thread.thread_id, multi_agent_version))
    }
}
