//! Publication custody for compaction items and session/subagent start hooks.
//! This tracks the live stream, not hook execution or replacement-history commits.

use super::*;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookStartedEvent;
use std::future::Future;
use std::future::poll_fn;
use std::task::Poll;
use tokio::sync::Notify;
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) struct StartHookInvocation(u64);

#[derive(Clone, Hash, PartialEq, Eq)]
enum PairKey {
    Compaction(String),
    StartHook(StartHookInvocation, String),
}

#[derive(Default)]
pub(crate) struct PairedLifecycle {
    next_invocation: AtomicU64,
    state: std::sync::Mutex<State>,
    changed: Notify,
    starts_sealed: CancellationToken,
}

#[derive(Default)]
struct State {
    sealed: bool,
    frozen: bool,
    next_job: u64,
    jobs: HashMap<u64, AbortHandle>,
    open: IndexMap<PairKey, EventMsg>,
    unacknowledged_append: bool,
    append_without_live_start: bool,
    unconfirmed_publication: bool,
    timed_out: bool,
    terminal_already_selected: bool,
}

enum Publication {
    Start(PairKey),
    Close,
}

struct Job {
    custody: Arc<PairedLifecycle>,
    id: u64,
    finished: bool,
}

impl Drop for Job {
    fn drop(&mut self) {
        let mut state = self.custody.state();
        state.jobs.remove(&self.id);
        if !self.finished {
            state.unconfirmed_publication = true;
        }
        self.custody.changed.notify_one();
    }
}

impl PairedLifecycle {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn start_hook_invocation(&self) -> StartHookInvocation {
        StartHookInvocation(self.next_invocation.fetch_add(1, Ordering::Relaxed))
    }

    /// Do not wait for the worker to drop its future: synchronous work can
    /// outlast forced cancellation. Channel admission observes this same lock.
    pub(crate) fn seal(&self) {
        self.state().sealed = true;
        self.starts_sealed.cancel();
    }

    async fn wait_for_jobs(&self) {
        loop {
            let changed = self.changed.notified();
            if self.state().jobs.is_empty() {
                return;
            }
            changed.await;
        }
    }

    /// Close only live, unclaimed starts. An exact close owns its original
    /// append, live event and any success-only legacy event from submission;
    /// never retry it (or replace it with a fallback) on ambiguous delivery.
    pub(crate) async fn settle(
        self: &Arc<Self>,
        session: &Session,
        turn_id: &str,
        budget: Duration,
        terminal_already_selected: bool,
    ) -> Option<String> {
        self.seal();
        // A fail-safe entered after terminal selection cannot assume that
        // either terminal sink is still untouched. Do not issue repairs then.
        let timed_out = if terminal_already_selected {
            false
        } else {
            tokio::time::timeout(budget, async {
                self.wait_for_jobs().await;
                loop {
                    {
                        let mut state = self.state();
                        if state.frozen {
                            break;
                        }
                        let Some((_, started)) = state.open.shift_remove_index(0) else {
                            break;
                        };
                        drop(self.spawn(
                            &mut state,
                            session,
                            turn_id,
                            Publication::Close,
                            unreported_completion(started),
                            Vec::new(),
                        ));
                    }
                    self.wait_for_jobs().await;
                }
            })
            .await
            .is_err()
        };

        let (jobs, warning) = {
            let mut state = self.state();
            if terminal_already_selected
                && !state.frozen
                && (!state.jobs.is_empty() || !state.open.is_empty())
            {
                state.terminal_already_selected = true;
            }
            state.frozen = true;
            state.timed_out |= timed_out;
            let jobs = std::mem::take(&mut state.jobs);
            let mut gaps = Vec::new();
            if state.timed_out {
                gaps.push("the settlement deadline expired; remaining publications were not confirmed");
            }
            if state.terminal_already_selected {
                gaps.push("the terminal was already selected; remaining publications were not attempted");
            }
            if state.unacknowledged_append {
                gaps.push("an ordered rollout append was not acknowledged");
            }
            if state.append_without_live_start {
                gaps.push("a start may have been recorded without live acceptance; no fallback close was emitted for that start");
            }
            if state.unconfirmed_publication {
                gaps.push("a paired publication did not finish or its close was not accepted live");
            }
            let warning = (!gaps.is_empty()).then(|| {
                format!(
                    "Turn {turn_id}: compaction/start-hook lifecycle publication unresolved: {}.",
                    gaps.join("; ")
                )
            });
            (jobs, warning)
        };
        for (_, job) in jobs {
            job.abort();
        }
        warning
    }

    async fn submit(
        self: &Arc<Self>,
        session: &Session,
        turn_id: &str,
        key: PairKey,
        msg: EventMsg,
        legacy: Vec<EventMsg>,
    ) {
        let job = {
            let mut state = self.state();
            if state.sealed {
                return;
            }
            if matches!(&msg, EventMsg::ItemStarted(_) | EventMsg::HookStarted(_)) {
                self.spawn(&mut state, session, turn_id, Publication::Start(key), msg, legacy)
            } else {
                let job = self.spawn(&mut state, session, turn_id, Publication::Close, msg, legacy);
                // From here the owned job, not the cancellable worker, owes
                // the exact close. This is not a receipt for either sink.
                state.open.shift_remove(&key);
                job
            }
        };
        if let Err(error) = job.await
            && error.is_panic()
        {
            std::panic::resume_unwind(error.into_panic());
        }
    }

    fn spawn(
        self: &Arc<Self>,
        state: &mut State,
        session: &Session,
        turn_id: &str,
        publication: Publication,
        msg: EventMsg,
        legacy: Vec<EventMsg>,
    ) -> tokio::task::JoinHandle<()> {
        let id = state.next_job;
        state.next_job += 1;
        let job = Job {
            custody: Arc::clone(self),
            id,
            finished: false,
        };
        let channel = session.tx_event.clone();
        let rollout = session.live_thread().cloned();
        let turn_id = turn_id.to_string();
        let handle = tokio::spawn(
            async move {
                let mut job = job;
                job.custody
                    .publish(&channel, rollout.as_ref(), &turn_id, publication, msg, legacy)
                    .await;
                job.finished = true;
            }
            .instrument(tracing::Span::current()),
        );
        state.jobs.insert(id, handle.abort_handle());
        handle
    }

    async fn publish(
        &self,
        channel: &Sender<Event>,
        rollout: Option<&LiveThread>,
        turn_id: &str,
        publication: Publication,
        msg: EventMsg,
        legacy: Vec<EventMsg>,
    ) {
        let start = match publication {
            Publication::Start(key) => Some((key, msg.clone())),
            Publication::Close => None,
        };
        {
            let state = self.state();
            if state.frozen || (start.is_some() && state.sealed) {
                return;
            }
        }
        let is_start = start.is_some();
        let append_attempted = self.append(rollout, &msg).await;
        if !self.send(channel, turn_id, msg, start).await {
            debug!(%turn_id, "paired lifecycle event was not accepted live");
            let mut state = self.state();
            if is_start {
                // Append acknowledgement is neither live acceptance nor durability.
                state.append_without_live_start |= append_attempted;
            } else {
                state.unconfirmed_publication = true;
            }
        }
        for msg in legacy {
            if self.state().frozen {
                return;
            }
            self.append(rollout, &msg).await;
            if !self.send(channel, turn_id, msg, None).await {
                self.state().unconfirmed_publication = true;
            }
        }
    }

    async fn append(&self, rollout: Option<&LiveThread>, msg: &EventMsg) -> bool {
        if let Some(rollout) = rollout.filter(|rollout| rollout.should_persist_event(msg)) {
            if let Err(error) = rollout
                .append_items_ordered(&[RolloutItem::EventMsg(msg.clone())])
                .await
            {
                self.state().unacknowledged_append = true;
                error!("failed to record paired lifecycle rollout item: {error:#}");
            }
            true
        } else {
            false
        }
    }

    /// The required-event channel retains its original backpressure and size
    /// policy. Poll and record live acceptance under the seal lock, without
    /// holding it across a pending poll, recorder operation, or telemetry.
    async fn send(
        &self,
        channel: &Sender<Event>,
        turn_id: &str,
        msg: EventMsg,
        mut start: Option<(PairKey, EventMsg)>,
    ) -> bool {
        if let Some(metrics) = codex_otel::global() {
            let _ = metrics.histogram("codex.event_channel.occupancy", channel.len() as i64, &[]);
        }
        let _send_wait_timer =
            codex_otel::start_global_timer("codex.event_channel.send_wait.duration_ms", &[]);
        let mut send = std::pin::pin!(codex_mcp::event_queue::send_event(
            channel,
            Event {
                id: turn_id.to_string(),
                msg,
            },
        ));
        let is_start = start.is_some();
        let send = poll_fn(|cx| {
            let mut state = self.state();
            if state.frozen || (is_start && state.sealed) {
                return Poll::Ready(false);
            }
            match send.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    if let Some((key, started)) = start.take() {
                        state.open.insert(key, started);
                    }
                    Poll::Ready(true)
                }
                Poll::Ready(Err(_)) => Poll::Ready(false),
                Poll::Pending => Poll::Pending,
            }
        });
        if is_start {
            tokio::select! {
                _ = self.starts_sealed.cancelled() => false,
                sent = send => sent,
            }
        } else {
            send.await
        }
    }
}

fn unreported_completion(started: EventMsg) -> EventMsg {
    match started {
        EventMsg::ItemStarted(started) => EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: started.thread_id,
            turn_id: started.turn_id,
            // The start contains only the item identity, not replacement metadata.
            item: started.item,
            completed_at_ms: now_unix_timestamp_ms(),
        }),
        EventMsg::HookStarted(started) => {
            let mut run = started.run;
            run.status = HookRunStatus::Stopped;
            run.status_message = Some("outcome not reported before abort".to_string());
            // Do not assert a process exit time, duration or hook-produced effects.
            EventMsg::HookCompleted(HookCompletedEvent {
                turn_id: started.turn_id,
                run,
            })
        }
        _ => unreachable!("only paired start events can be open"),
    }
}

impl Session {
    pub(crate) async fn emit_compaction_started(
        &self,
        turn_context: &TurnContext,
        item: &ContextCompactionItem,
    ) {
        turn_context
            .paired_lifecycle()
            .submit(
                self,
                &turn_context.sub_id,
                PairKey::Compaction(item.id.clone()),
                EventMsg::ItemStarted(ItemStartedEvent {
                    thread_id: self.thread_id,
                    turn_id: turn_context.sub_id.clone(),
                    item: TurnItem::ContextCompaction(item.clone()),
                    started_at_ms: now_unix_timestamp_ms(),
                }),
                Vec::new(),
            )
            .await;
    }

    pub(crate) async fn emit_compaction_completed(
        &self,
        turn_context: &TurnContext,
        item: ContextCompactionItem,
        emit_legacy: bool,
    ) {
        let key = PairKey::Compaction(item.id.clone());
        let item = TurnItem::ContextCompaction(item);
        record_turn_ttfm_metric(turn_context, &item);
        let msg = EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: self.thread_id,
            turn_id: turn_context.sub_id.clone(),
            item,
            completed_at_ms: now_unix_timestamp_ms(),
        });
        let legacy = if emit_legacy {
            msg.as_legacy_events(self.show_raw_agent_reasoning())
        } else {
            Vec::new()
        };
        turn_context
            .paired_lifecycle()
            .submit(self, &turn_context.sub_id, key, msg, legacy)
            .await;
    }

    pub(crate) async fn emit_scoped_start_hook_started(
        &self,
        turn_context: &TurnContext,
        invocation: StartHookInvocation,
        event: HookStartedEvent,
    ) {
        let key = PairKey::StartHook(invocation, event.run.id.clone());
        turn_context
            .paired_lifecycle()
            .submit(
                self,
                &turn_context.sub_id,
                key,
                EventMsg::HookStarted(event),
                Vec::new(),
            )
            .await;
    }

    pub(crate) async fn emit_scoped_start_hook_completed(
        &self,
        turn_context: &TurnContext,
        invocation: StartHookInvocation,
        event: HookCompletedEvent,
    ) {
        let key = PairKey::StartHook(invocation, event.run.id.clone());
        turn_context
            .paired_lifecycle()
            .submit(
                self,
                &turn_context.sub_id,
                key,
                EventMsg::HookCompleted(event),
                Vec::new(),
            )
            .await;
    }
}
