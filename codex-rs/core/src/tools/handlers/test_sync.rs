use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::sleep;

use crate::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::test_sync_spec::create_test_sync_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

pub struct TestSyncHandler;

const DEFAULT_TIMEOUT_MS: u64 = 1_000;

type BarrierKey = (String, String, String);

static BARRIERS: OnceLock<Mutex<HashMap<BarrierKey, BarrierState>>> = OnceLock::new();

struct BarrierState {
    generation: Arc<BarrierGeneration>,
    waiters: usize,
}

struct BarrierGeneration {
    participants: usize,
    released: watch::Sender<bool>,
}

struct BarrierWaiter {
    id: BarrierKey,
    generation: Arc<BarrierGeneration>,
    released: watch::Receiver<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BarrierArgs {
    id: String,
    participants: usize,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestSyncArgs {
    #[serde(default)]
    sleep_before_ms: Option<u64>,
    #[serde(default)]
    sleep_after_ms: Option<u64>,
    #[serde(default)]
    barrier: Option<BarrierArgs>,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

fn barrier_map() -> &'static Mutex<HashMap<BarrierKey, BarrierState>> {
    BARRIERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_barrier_map() -> std::sync::MutexGuard<'static, HashMap<BarrierKey, BarrierState>> {
    barrier_map()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl BarrierWaiter {
    async fn wait(mut self) -> Result<(), watch::error::RecvError> {
        self.released.wait_for(|released| *released).await?;
        Ok(())
    }
}

impl Drop for BarrierWaiter {
    fn drop(&mut self) {
        let mut map = lock_barrier_map();
        let remove_generation = if let Some(state) = map.get_mut(&self.id)
            && Arc::ptr_eq(&state.generation, &self.generation)
        {
            debug_assert!(state.waiters > 0);
            if state.waiters <= 1 {
                true
            } else {
                state.waiters -= 1;
                false
            }
        } else {
            false
        };
        if remove_generation {
            map.remove(&self.id);
        }
    }
}

impl ToolExecutor<ToolInvocation> for TestSyncHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("test_sync_tool")
    }

    fn spec(&self) -> ToolSpec {
        create_test_sync_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl TestSyncHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "test_sync_tool handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: TestSyncArgs = parse_arguments(&arguments)?;

        if let Some(delay) = args.sleep_before_ms
            && delay > 0
        {
            sleep(Duration::from_millis(delay)).await;
        }

        if let Some(barrier) = args.barrier {
            wait_on_barrier(
                (&session.thread_id().to_string(), &step_context.turn.sub_id),
                barrier,
            )
            .await?;
        }

        if let Some(delay) = args.sleep_after_ms
            && delay > 0
        {
            sleep(Duration::from_millis(delay)).await;
        }

        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            "ok".to_string(),
            Some(true),
        )))
    }
}

impl CoreToolRuntime for TestSyncHandler {}

async fn wait_on_barrier(scope: (&str, &str), args: BarrierArgs) -> Result<(), FunctionCallError> {
    if args.participants == 0 {
        return Err(FunctionCallError::RespondToModel(
            "barrier participants must be greater than zero".to_string(),
        ));
    }

    if args.timeout_ms == 0 {
        return Err(FunctionCallError::RespondToModel(
            "barrier timeout must be greater than zero".to_string(),
        ));
    }

    let timeout = Duration::from_millis(args.timeout_ms);
    let waiter = register_barrier(scope, &args)?;
    tokio::time::timeout(timeout, waiter.wait())
        .await
        .map_err(|_| {
            FunctionCallError::RespondToModel("test_sync_tool barrier wait timed out".to_string())
        })?
        .map_err(|_| {
            FunctionCallError::RespondToModel(
                "test_sync_tool barrier generation ended unexpectedly".to_string(),
            )
        })?;

    Ok(())
}

fn register_barrier(
    scope: (&str, &str),
    args: &BarrierArgs,
) -> Result<BarrierWaiter, FunctionCallError> {
    let barrier_id = (scope.0.to_string(), scope.1.to_string(), args.id.clone());
    let mut map = lock_barrier_map();
    let generation = if let Some(state) = map.get_mut(&barrier_id) {
        if state.generation.participants != args.participants {
            let existing = state.generation.participants;
            return Err(FunctionCallError::RespondToModel(format!(
                "barrier {} already registered with {existing} participants",
                args.id
            )));
        }
        state.waiters += 1;
        Arc::clone(&state.generation)
    } else {
        let (released, _) = watch::channel(false);
        let generation = Arc::new(BarrierGeneration {
            participants: args.participants,
            released,
        });
        map.insert(
            barrier_id.clone(),
            BarrierState {
                generation: Arc::clone(&generation),
                waiters: 1,
            },
        );
        generation
    };
    let released = generation.released.subscribe();
    let should_release = map
        .get(&barrier_id)
        .is_some_and(|state| state.waiters == state.generation.participants);
    if should_release {
        let Some(removed) = map.remove(&barrier_id) else {
            return Err(FunctionCallError::RespondToModel(
                "registered barrier generation disappeared".to_string(),
            ));
        };
        debug_assert!(Arc::ptr_eq(&removed.generation, &generation));
    }
    drop(map);

    if should_release {
        generation.released.send_replace(true);
    }

    Ok(BarrierWaiter {
        id: barrier_id,
        generation,
        released,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::step_context::StepContext;
    use crate::session::tests::make_session_and_context;
    use crate::tools::context::ToolCallSource;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use tokio_util::sync::CancellationToken;

    const TEST_SCOPE: (&str, &str) = ("test-thread", "test-turn");

    static NEXT_BARRIER_ID: AtomicU64 = AtomicU64::new(1);

    fn unique_barrier_id(label: &str) -> String {
        let suffix = NEXT_BARRIER_ID.fetch_add(1, Ordering::Relaxed);
        format!("test-{label}-{suffix}")
    }

    fn barrier_args(id: &str, participants: usize, timeout_ms: u64) -> BarrierArgs {
        BarrierArgs {
            id: id.to_string(),
            participants,
            timeout_ms,
        }
    }

    fn registered_waiters(id: &str) -> Option<usize> {
        lock_barrier_map()
            .get(&(
                TEST_SCOPE.0.to_string(),
                TEST_SCOPE.1.to_string(),
                id.to_string(),
            ))
            .map(|state| state.waiters)
    }

    async fn wait_until_registered(id: &str, waiters: usize) {
        for _ in 0..1_000 {
            if registered_waiters(id) == Some(waiters) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("barrier {id} did not register {waiters} waiter(s)");
    }

    async fn assert_fresh_pair_rendezvous(id: &str) {
        let first_id = id.to_string();
        let first = tokio::spawn(async move {
            wait_on_barrier(TEST_SCOPE, barrier_args(&first_id, 2, 5_000)).await
        });
        wait_until_registered(id, 1).await;
        assert!(!first.is_finished());

        let second_id = id.to_string();
        let second = tokio::spawn(async move {
            wait_on_barrier(TEST_SCOPE, barrier_args(&second_id, 2, 5_000)).await
        });
        let (first, second) = tokio::join!(first, second);
        assert!(first.expect("first rendezvous task").is_ok());
        assert!(second.expect("second rendezvous task").is_ok());
        assert_eq!(registered_waiters(id), None);
    }

    #[tokio::test]
    async fn handler_scopes_same_id_barriers_to_session_and_turn() {
        let (session, mut first_turn) = make_session_and_context().await;
        let (other_session, mut other_session_turn) = make_session_and_context().await;
        let (_, mut next_turn) = make_session_and_context().await;
        first_turn.sub_id = "first-turn".to_string();
        other_session_turn.sub_id = first_turn.sub_id.clone();
        next_turn.sub_id = "next-turn".to_string();
        let session = Arc::new(session);
        let other_session = Arc::new(other_session);
        assert_ne!(session.thread_id(), other_session.thread_id());
        let id = unique_barrier_id("scoped-handler");
        let invocation = |session, turn| ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::new(turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "barrier-call".to_string(),
            tool_name: ToolName::plain("test_sync_tool"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: serde_json::json!({
                    "barrier": {"id": id, "participants": 2, "timeout_ms": 5_000}
                })
                .to_string(),
            },
        };
        let invocations = [
            invocation(Arc::clone(&session), first_turn),
            invocation(other_session, other_session_turn),
            invocation(session, next_turn),
        ];
        let handler = TestSyncHandler;
        let mut waiting = invocations
            .iter()
            .map(|invocation| handler.handle(invocation.clone()))
            .collect::<Vec<_>>();
        for waiter in &mut waiting {
            assert!(
                futures::poll!(waiter.as_mut()).is_pending(),
                "other scopes must not release this waiter"
            );
        }
        for (index, invocation) in invocations.into_iter().enumerate() {
            let partner = handler.handle(invocation).await.expect("matching partner");
            assert_eq!(partner.log_preview(), "ok");
            assert!(partner.success_for_logging());
            let released = waiting[index]
                .as_mut()
                .await
                .expect("matching waiter released");
            assert_eq!(released.log_preview(), "ok");
            assert!(released.success_for_logging());
            for waiter in &mut waiting[index + 1..] {
                assert!(futures::poll!(waiter.as_mut()).is_pending());
            }
        }
        assert!(!lock_barrier_map().keys().any(|key| key.2 == id));
    }

    #[tokio::test]
    async fn timed_out_waiter_does_not_satisfy_a_later_rendezvous() {
        let id = unique_barrier_id("timeout");
        let error = wait_on_barrier(TEST_SCOPE, barrier_args(&id, 2, 10))
            .await
            .expect_err("single waiter must time out");
        assert!(error.to_string().contains("barrier wait timed out"));
        assert_eq!(registered_waiters(&id), None);

        assert_fresh_pair_rendezvous(&id).await;
    }

    #[tokio::test]
    async fn aborted_waiter_does_not_satisfy_a_later_rendezvous() {
        let id = unique_barrier_id("abort");
        let aborted_id = id.clone();
        let task = tokio::spawn(async move {
            wait_on_barrier(TEST_SCOPE, barrier_args(&aborted_id, 2, 5_000)).await
        });
        wait_until_registered(&id, 1).await;

        task.abort();
        assert!(task.await.expect_err("task must be aborted").is_cancelled());
        assert_eq!(registered_waiters(&id), None);

        assert_fresh_pair_rendezvous(&id).await;
    }

    #[tokio::test]
    async fn consecutive_same_id_generations_are_disjoint() {
        let id = unique_barrier_id("generation");
        let first =
            register_barrier(TEST_SCOPE, &barrier_args(&id, 2, 5_000)).expect("first waiter");
        let first_generation = Arc::clone(&first.generation);
        let second =
            register_barrier(TEST_SCOPE, &barrier_args(&id, 2, 5_000)).expect("second waiter");
        assert_eq!(registered_waiters(&id), None);

        let next_first =
            register_barrier(TEST_SCOPE, &barrier_args(&id, 2, 5_000)).expect("next first waiter");
        assert!(!Arc::ptr_eq(&first_generation, &next_first.generation));
        assert_eq!(registered_waiters(&id), Some(1));
        let next_second =
            register_barrier(TEST_SCOPE, &barrier_args(&id, 2, 5_000)).expect("next second waiter");
        assert_eq!(registered_waiters(&id), None);

        let (first, second, next_first, next_second) = tokio::join!(
            first.wait(),
            second.wait(),
            next_first.wait(),
            next_second.wait(),
        );
        assert!(first.is_ok());
        assert!(second.is_ok());
        assert!(next_first.is_ok());
        assert!(next_second.is_ok());
    }
}
