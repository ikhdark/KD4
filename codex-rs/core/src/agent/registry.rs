use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use rand::prelude::IndexedRandom;
use rand::prelude::IteratorRandom;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// This structure is used to add some limits on the multi-agent capabilities for Codex. In
/// the current implementation, it limits:
/// * Total number of sub-agents (i.e. threads) per user session
///
/// This structure is shared by all agents in the same user session (because the `AgentControl`
/// is).
pub(crate) struct AgentRegistry {
    active_agents: Mutex<ActiveAgents>,
    total_count: AtomicUsize,
    capacity_updates: tokio::sync::watch::Sender<u64>,
    /// Latches on the first registered sub-agent and never clears.
    ///
    /// `has_live_agents` reflects the current graph, which empties when children
    /// finish. The agent tool surface is derived from this flag instead so those
    /// tool schemas — part of the cached prompt prefix — cannot be retracted and
    /// re-added as children come and go.
    ever_had_live_agents: AtomicBool,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self {
            active_agents: Mutex::default(),
            total_count: AtomicUsize::new(0),
            capacity_updates: tokio::sync::watch::channel(0).0,
            ever_had_live_agents: AtomicBool::new(false),
        }
    }
}

#[derive(Default)]
struct ActiveAgents {
    agent_tree: HashMap<String, AgentMetadata>,
    // Residency is not lineage: keep the edge through cold/pathless intermediates.
    parents: HashMap<ThreadId, ThreadId>,
    // Ephemeral turn ownership retires with its existing lineage edge.
    turn_spawn_owners: HashMap<ThreadId, (ThreadId, String)>,
    used_agent_nicknames: HashSet<String>,
    nickname_reset_count: usize,
    closing_threads: HashMap<ThreadId, usize>,
    parent_spawn_reservations: HashMap<ThreadId, usize>,
    // Mail is logical-agent state, not resident-runtime state. Reuse the existing
    // bounded queue and dedup owner instead of transferring messages on reload.
    input_queues: HashMap<ThreadId, Arc<crate::session::InputQueue>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentMetadata {
    pub(crate) agent_id: Option<ThreadId>,
    pub(crate) agent_path: Option<AgentPath>,
    pub(crate) agent_nickname: Option<String>,
    pub(crate) agent_role: Option<String>,
    pub(crate) last_task_message: Option<String>,
}

fn format_agent_nickname(name: &str, nickname_reset_count: usize) -> String {
    match nickname_reset_count {
        0 => name.to_string(),
        reset_count => {
            let value = reset_count + 1;
            let suffix = match value % 100 {
                11..=13 => "th",
                _ => match value % 10 {
                    1 => "st", // codespell:ignore
                    2 => "nd", // codespell:ignore
                    3 => "rd", // codespell:ignore
                    _ => "th", // codespell:ignore
                },
            };
            format!("{name} the {value}{suffix}")
        }
    }
}

fn session_depth(session_source: &SessionSource) -> i32 {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => *depth,
        SessionSource::SubAgent(_) => 0,
        _ => 0,
    }
}

pub(crate) fn next_thread_spawn_depth(session_source: &SessionSource) -> i32 {
    session_depth(session_source).saturating_add(1)
}

pub(crate) fn exceeds_thread_spawn_depth_limit(depth: i32, max_depth: i32) -> bool {
    depth > max_depth
}

impl AgentRegistry {
    pub(crate) fn subscribe_capacity_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.capacity_updates.subscribe()
    }

    pub(crate) fn notify_execution_capacity_released(&self) {
        self.capacity_updates
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    pub(crate) fn reserve_spawn_slot(
        self: &Arc<Self>,
        max_threads: Option<usize>,
    ) -> Result<SpawnReservation> {
        if let Some(max_threads) = max_threads {
            if !self.try_increment_spawned(max_threads) {
                return Err(CodexErr::AgentLimitReached { max_threads });
            }
        } else {
            self.total_count.fetch_add(1, Ordering::AcqRel);
        }
        Ok(SpawnReservation {
            state: Arc::clone(self),
            active: true,
            reserved_agent_path: None,
            parent_thread_id: None,
        })
    }

    pub(crate) fn begin_closing_agent_tree(
        self: &Arc<Self>,
        root_thread_id: ThreadId,
    ) -> AgentTreeClosingGuard {
        let mut guard = AgentTreeClosingGuard {
            state: Arc::clone(self),
            thread_ids: HashSet::new(),
        };
        guard.mark_threads([root_thread_id]);
        guard
    }

    pub(crate) fn release_spawned_thread(&self, thread_id: ThreadId) {
        let removed_counted_agent = {
            let mut active_agents = self
                .active_agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let removed_key = active_agents
                .agent_tree
                .iter()
                .find_map(|(key, metadata)| (metadata.agent_id == Some(thread_id)).then_some(key))
                .cloned();
            active_agents.input_queues.remove(&thread_id);
            if !active_agents.parents.contains_key(&thread_id) {
                active_agents.turn_spawn_owners.remove(&thread_id);
            }
            let removed = removed_key
                .and_then(|key| active_agents.agent_tree.remove(key.as_str()))
                .is_some_and(|metadata| {
                    !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
                });
            // Cold ancestors stay reachable, but explicitly closed leaf branches
            // must not retain one edge per historical spawn forever.
            let mut retired = thread_id;
            while !active_agents
                .agent_tree
                .values()
                .any(|agent| agent.agent_id == Some(retired))
                && !active_agents
                    .parents
                    .values()
                    .any(|parent| *parent == retired)
            {
                let Some(parent) = active_agents.parents.remove(&retired) else {
                    break;
                };
                active_agents.turn_spawn_owners.remove(&retired);
                retired = parent;
            }
            removed
        };
        if removed_counted_agent {
            self.total_count.fetch_sub(1, Ordering::AcqRel);
            self.capacity_updates
                .send_modify(|generation| *generation = generation.wrapping_add(1));
        }
    }

    pub(crate) fn register_root_thread(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .agent_tree
            .entry(AgentPath::ROOT.to_string())
            .or_insert_with(|| AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: Some(AgentPath::root()),
                ..Default::default()
            });
    }

    pub(crate) fn retained_input_queue(
        &self,
        id: ThreadId,
    ) -> Option<Arc<crate::session::InputQueue>> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .input_queues
            .get(&id)
            .cloned()
    }

    pub(crate) fn remember_input_queue(
        &self,
        id: ThreadId,
        queue: Arc<crate::session::InputQueue>,
    ) {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .input_queues
            .entry(id)
            .or_insert(queue);
    }

    pub(crate) fn input_queue_for_delivery(
        &self,
        id: ThreadId,
    ) -> Option<Arc<crate::session::InputQueue>> {
        let active = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.closing_threads.contains_key(&id)
            || !active
                .agent_tree
                .values()
                .any(|agent| agent.agent_id == Some(id))
        {
            return None;
        }
        active.input_queues.get(&id).cloned()
    }

    pub(crate) fn agent_id_for_path(&self, agent_path: &AgentPath) -> Option<ThreadId> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .get(agent_path.as_str())
            .and_then(|metadata| metadata.agent_id)
    }

    pub(crate) fn agent_metadata_for_thread(&self, thread_id: ThreadId) -> Option<AgentMetadata> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .values()
            .find(|metadata| metadata.agent_id == Some(thread_id))
            .cloned()
    }

    pub(crate) fn agent_metadata_for_threads(
        &self,
        thread_ids: &HashSet<ThreadId>,
    ) -> HashMap<ThreadId, AgentMetadata> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .values()
            .filter_map(|metadata| {
                let id = metadata.agent_id?;
                thread_ids.contains(&id).then(|| (id, metadata.clone()))
            })
            .collect()
    }

    pub(crate) fn live_agents(&self) -> Vec<AgentMetadata> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .values()
            .filter(|metadata| {
                metadata.agent_id.is_some()
                    && !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn has_live_agents(&self) -> bool {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .values()
            .any(|metadata| {
                metadata.agent_id.is_some()
                    && !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
            })
    }

    /// Whether a sub-agent has ever been live in this session tree.
    ///
    /// Monotonic: once true it stays true for the life of the registry, so the
    /// agent tool surface derived from it stays byte-stable in the cached prefix.
    pub(crate) fn ever_had_live_agents(&self) -> bool {
        if self.ever_had_live_agents.load(Ordering::Acquire) {
            return true;
        }
        // Latch lazily too, so a tree whose children were registered by a path
        // other than `register_spawned_thread` (a resumed session, for example)
        // still reports the surface.
        if self.has_live_agents() {
            self.ever_had_live_agents.store(true, Ordering::Release);
            return true;
        }
        false
    }

    pub(crate) fn update_last_task_message(&self, thread_id: ThreadId, last_task_message: String) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(metadata) = active_agents
            .agent_tree
            .values_mut()
            .find(|metadata| metadata.agent_id == Some(thread_id))
        {
            metadata.last_task_message = Some(bounded_task_message(last_task_message));
        }
    }

    pub(crate) fn clear_last_task_message(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(metadata) = active_agents
            .agent_tree
            .values_mut()
            .find(|metadata| metadata.agent_id == Some(thread_id))
        {
            metadata.last_task_message = None;
        }
    }

    fn register_spawned_thread(
        &self,
        mut agent_metadata: AgentMetadata,
        parent_thread_id: Option<ThreadId>,
    ) -> Result<()> {
        let Some(thread_id) = agent_metadata.agent_id else {
            return Err(CodexErr::Fatal(
                "cannot register a spawned agent without a thread id".to_string(),
            ));
        };
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(parent_thread_id) = parent_thread_id
            && active_agents
                .closing_threads
                .contains_key(&parent_thread_id)
        {
            return Err(parent_closing_error(parent_thread_id));
        }
        agent_metadata.last_task_message =
            agent_metadata.last_task_message.map(bounded_task_message);
        let key = agent_metadata
            .agent_path
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("thread:{thread_id}"));
        if active_agents
            .agent_tree
            .values()
            .any(|metadata| metadata.agent_id == Some(thread_id))
        {
            return Err(CodexErr::UnsupportedOperation(format!(
                "agent thread {thread_id} is already registered"
            )));
        }
        if active_agents
            .agent_tree
            .get(&key)
            .is_some_and(|metadata| metadata.agent_id.is_some())
        {
            return Err(CodexErr::UnsupportedOperation(format!(
                "agent path `{key}` already exists"
            )));
        }
        if let Some(agent_nickname) = agent_metadata.agent_nickname.clone() {
            active_agents.used_agent_nicknames.insert(agent_nickname);
        }
        // Latch here, matching the non-root filter `has_live_agents` applies, so
        // the agent tool surface survives a child that finishes before the next
        // turn assembles its prompt.
        if !agent_metadata
            .agent_path
            .as_ref()
            .is_some_and(AgentPath::is_root)
        {
            self.ever_had_live_agents.store(true, Ordering::Release);
        }
        if let Some(parent) = parent_thread_id {
            active_agents.parents.insert(thread_id, parent);
        }
        active_agents.agent_tree.insert(key, agent_metadata);
        Ok(())
    }

    pub(crate) fn known_spawn_edges(&self) -> Vec<(ThreadId, ThreadId)> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .parents
            .iter()
            .map(|(child, parent)| (*parent, *child))
            .collect()
    }

    pub(crate) fn record_turn_spawn_owner(&self, child: ThreadId, parent: ThreadId, turn: String) {
        self.active_agents.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .turn_spawn_owners.insert(child, (parent, turn));
    }

    pub(crate) fn turn_spawn_owners(&self) -> HashMap<ThreadId, (ThreadId, String)> {
        self.active_agents.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .turn_spawn_owners.clone()
    }

    pub(crate) fn restore_cold_agent(
        &self,
        metadata: AgentMetadata,
        parent: ThreadId,
    ) -> Result<()> {
        let thread_id = metadata
            .agent_id
            .ok_or_else(|| CodexErr::Fatal("cold agent has no thread id".into()))?;
        let path = metadata
            .agent_path
            .as_ref()
            .ok_or_else(|| CodexErr::Fatal("cold agent has no path".into()))?
            .to_string();
        let mut active = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.closing_threads.contains_key(&parent)
            || active.closing_threads.contains_key(&thread_id)
        {
            return Err(parent_closing_error(parent));
        }
        if let Some(existing) = active.agent_tree.get(&path) {
            return if existing.agent_id == Some(thread_id) {
                Ok(())
            } else {
                Err(CodexErr::Fatal(
                    "cold agent path is owned by another thread".into(),
                ))
            };
        }
        if active
            .agent_tree
            .values()
            .any(|entry| entry.agent_id == Some(thread_id))
        {
            return Err(CodexErr::Fatal(
                "cold agent thread is registered at another path".into(),
            ));
        }
        active.parents.insert(thread_id, parent);
        active.agent_tree.insert(path, metadata);
        self.total_count.fetch_add(1, Ordering::AcqRel);
        self.ever_had_live_agents.store(true, Ordering::Release);
        Ok(())
    }

    fn ensure_parent_open(&self, parent_thread_id: ThreadId) -> Result<()> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active_agents
            .closing_threads
            .contains_key(&parent_thread_id)
        {
            return Err(parent_closing_error(parent_thread_id));
        }
        let count = active_agents
            .parent_spawn_reservations
            .entry(parent_thread_id)
            .or_default();
        *count = count.saturating_add(1);
        Ok(())
    }

    fn release_parent_spawn_reservation(&self, parent_thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remove_reservation = match active_agents
            .parent_spawn_reservations
            .entry(parent_thread_id)
        {
            Entry::Occupied(mut entry) if *entry.get() > 1 => {
                *entry.get_mut() -= 1;
                false
            }
            Entry::Occupied(entry) => {
                entry.remove();
                true
            }
            Entry::Vacant(_) => false,
        };
        if remove_reservation
            && active_agents
                .closing_threads
                .get(&parent_thread_id)
                .is_some_and(|guard_count| *guard_count == 0)
        {
            active_agents.closing_threads.remove(&parent_thread_id);
        }
    }

    fn mark_threads_closing(&self, thread_ids: &[ThreadId]) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for thread_id in thread_ids {
            let count = active_agents.closing_threads.entry(*thread_id).or_default();
            *count = count.saturating_add(1);
        }
    }

    fn release_closing_threads(&self, thread_ids: &HashSet<ThreadId>) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for thread_id in thread_ids {
            let has_pending_spawn = active_agents
                .parent_spawn_reservations
                .contains_key(thread_id);
            match active_agents.closing_threads.entry(*thread_id) {
                Entry::Occupied(mut entry) if *entry.get() > 1 => {
                    *entry.get_mut() -= 1;
                }
                Entry::Occupied(mut entry) => {
                    if has_pending_spawn {
                        *entry.get_mut() = 0;
                    } else {
                        entry.remove();
                    }
                }
                Entry::Vacant(_) => {}
            }
        }
    }

    fn reserve_agent_nickname(&self, names: &[&str], preferred: Option<&str>) -> Option<String> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let agent_nickname = if let Some(preferred) = preferred {
            preferred.to_string()
        } else {
            if names.is_empty() {
                return None;
            }
            let available_name = names
                .iter()
                .map(|name| format_agent_nickname(name, active_agents.nickname_reset_count))
                .filter(|name| !active_agents.used_agent_nicknames.contains(name))
                .choose(&mut rand::rng());
            if let Some(name) = available_name {
                name
            } else {
                active_agents.used_agent_nicknames.clear();
                active_agents.nickname_reset_count += 1;
                if let Some(metrics) = codex_otel::global() {
                    let _ = metrics.counter(
                        "codex.multi_agent.nickname_pool_reset",
                        /*inc*/ 1,
                        &[],
                    );
                }
                format_agent_nickname(
                    names.choose(&mut rand::rng())?,
                    active_agents.nickname_reset_count,
                )
            }
        };
        active_agents
            .used_agent_nicknames
            .insert(agent_nickname.clone());
        Some(agent_nickname)
    }

    fn reserve_agent_path(&self, agent_path: &AgentPath) -> Result<()> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match active_agents.agent_tree.entry(agent_path.to_string()) {
            Entry::Occupied(_) => Err(CodexErr::UnsupportedOperation(format!(
                "agent path `{agent_path}` already exists"
            ))),
            Entry::Vacant(entry) => {
                entry.insert(AgentMetadata {
                    agent_path: Some(agent_path.clone()),
                    ..Default::default()
                });
                Ok(())
            }
        }
    }

    fn release_reserved_agent_path(&self, agent_path: &AgentPath) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active_agents
            .agent_tree
            .get(agent_path.as_str())
            .is_some_and(|metadata| metadata.agent_id.is_none())
        {
            active_agents.agent_tree.remove(agent_path.as_str());
        }
    }

    fn try_increment_spawned(&self, max_threads: usize) -> bool {
        let mut current = self.total_count.load(Ordering::Acquire);
        loop {
            if current >= max_threads {
                return false;
            }
            match self.total_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(updated) => current = updated,
            }
        }
    }
}

fn parent_closing_error(parent_thread_id: ThreadId) -> CodexErr {
    CodexErr::UnsupportedOperation(format!(
        "cannot spawn a child agent while parent agent {parent_thread_id} is closing"
    ))
}

pub(crate) struct AgentTreeClosingGuard {
    state: Arc<AgentRegistry>,
    thread_ids: HashSet<ThreadId>,
}

impl AgentTreeClosingGuard {
    pub(crate) fn mark_threads(&mut self, thread_ids: impl IntoIterator<Item = ThreadId>) {
        let newly_marked = thread_ids
            .into_iter()
            .filter(|thread_id| self.thread_ids.insert(*thread_id))
            .collect::<Vec<_>>();
        if !newly_marked.is_empty() {
            self.state.mark_threads_closing(&newly_marked);
        }
    }
}

impl Drop for AgentTreeClosingGuard {
    fn drop(&mut self) {
        self.state.release_closing_threads(&self.thread_ids);
    }
}

pub(crate) struct SpawnReservation {
    state: Arc<AgentRegistry>,
    active: bool,
    reserved_agent_path: Option<AgentPath>,
    parent_thread_id: Option<ThreadId>,
}

impl SpawnReservation {
    pub(crate) fn reserve_agent_nickname_with_preference(
        &mut self,
        names: &[&str],
        preferred: Option<&str>,
    ) -> Result<String> {
        let agent_nickname = self
            .state
            .reserve_agent_nickname(names, preferred)
            .ok_or_else(|| {
                CodexErr::UnsupportedOperation("no available agent nicknames".to_string())
            })?;
        Ok(agent_nickname)
    }

    pub(crate) fn reserve_agent_path(&mut self, agent_path: &AgentPath) -> Result<()> {
        self.state.reserve_agent_path(agent_path)?;
        self.reserved_agent_path = Some(agent_path.clone());
        Ok(())
    }

    pub(crate) fn reserve_parent_thread(&mut self, parent_thread_id: ThreadId) -> Result<()> {
        self.state.ensure_parent_open(parent_thread_id)?;
        self.parent_thread_id = Some(parent_thread_id);
        Ok(())
    }


    /// Keep failed admission owned by the caller until its live child terminates.
    pub(crate) fn try_commit(&mut self, agent_metadata: AgentMetadata) -> Result<()> {
        self.state
            .register_spawned_thread(agent_metadata, self.parent_thread_id)?;
        if let Some(parent_thread_id) = self.parent_thread_id.take() {
            self.state
                .release_parent_spawn_reservation(parent_thread_id);
        }
        self.reserved_agent_path = None;
        self.active = false;
        Ok(())
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if self.active {
            if let Some(parent_thread_id) = self.parent_thread_id.take() {
                self.state
                    .release_parent_spawn_reservation(parent_thread_id);
            }
            if let Some(agent_path) = self.reserved_agent_path.take() {
                self.state.release_reserved_agent_path(&agent_path);
            }
            self.state.total_count.fetch_sub(1, Ordering::AcqRel);
            self.state
                .capacity_updates
                .send_modify(|generation| *generation = generation.wrapping_add(1));
        }
    }
}

// Display metadata is bounded independently of the authoritative delivered message.
const MAX_TASK_MESSAGE_BYTES: usize = 2_048;

#[derive(Default)]
pub(super) struct TaskMessagePreview(String);

impl std::fmt::Write for TaskMessagePreview {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        // Four lookahead bytes prove overflow without splitting a UTF-8 scalar.
        let remaining = (MAX_TASK_MESSAGE_BYTES + 4).saturating_sub(self.0.len());
        let mut end = text.len().min(remaining);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push_str(&text[..end]);
        if end < text.len() || self.0.len() > MAX_TASK_MESSAGE_BYTES {
            Err(std::fmt::Error)
        } else {
            Ok(())
        }
    }
}

impl TaskMessagePreview {
    pub(super) fn finish(self) -> String {
        bounded_task_message(self.0)
    }
}

pub(super) fn bounded_task_message_from_text(text: &str) -> String {
    let mut preview = TaskMessagePreview::default();
    let _ = std::fmt::Write::write_str(&mut preview, text);
    preview.finish()
}

fn bounded_task_message(mut message: String) -> String {
    const SUFFIX: &str = "\n[truncated]";
    if message.len() > MAX_TASK_MESSAGE_BYTES {
        let mut end = MAX_TASK_MESSAGE_BYTES - SUFFIX.len();
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push_str(SUFFIX);
        message.shrink_to_fit();
    }
    message
}

