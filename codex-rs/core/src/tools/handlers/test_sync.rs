use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::sleep;

use crate::function_tool::FunctionCallError;
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

static BARRIERS: OnceLock<Mutex<HashMap<String, BarrierState>>> = OnceLock::new();

struct BarrierState {
    generation: Arc<BarrierGeneration>,
    waiters: usize,
}

struct BarrierGeneration {
    participants: usize,
    released: watch::Sender<bool>,
}

struct BarrierWaiter {
    id: String,
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

fn barrier_map() -> &'static Mutex<HashMap<String, BarrierState>> {
    BARRIERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_barrier_map() -> std::sync::MutexGuard<'static, HashMap<String, BarrierState>> {
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
        let ToolInvocation { payload, .. } = invocation;

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
            wait_on_barrier(barrier).await?;
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

async fn wait_on_barrier(args: BarrierArgs) -> Result<(), FunctionCallError> {
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
    let waiter = register_barrier(&args)?;
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

fn register_barrier(args: &BarrierArgs) -> Result<BarrierWaiter, FunctionCallError> {
    let barrier_id = args.id.clone();
    let mut map = lock_barrier_map();
    let generation = if let Some(state) = map.get_mut(&barrier_id) {
        if state.generation.participants != args.participants {
            let existing = state.generation.participants;
            return Err(FunctionCallError::RespondToModel(format!(
                "barrier {barrier_id} already registered with {existing} participants"
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
        let removed = map
            .remove(&barrier_id)
            .expect("registered barrier generation must be present");
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
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

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
        lock_barrier_map().get(id).map(|state| state.waiters)
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
        let first =
            tokio::spawn(async move { wait_on_barrier(barrier_args(&first_id, 2, 5_000)).await });
        wait_until_registered(id, 1).await;
        assert!(!first.is_finished());

        let second_id = id.to_string();
        let second =
            tokio::spawn(async move { wait_on_barrier(barrier_args(&second_id, 2, 5_000)).await });
        let (first, second) = tokio::join!(first, second);
        assert!(first.expect("first rendezvous task").is_ok());
        assert!(second.expect("second rendezvous task").is_ok());
        assert_eq!(registered_waiters(id), None);
    }

    #[tokio::test]
    async fn timed_out_waiter_does_not_satisfy_a_later_rendezvous() {
        let id = unique_barrier_id("timeout");
        let error = wait_on_barrier(barrier_args(&id, 2, 10))
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
        let task =
            tokio::spawn(async move { wait_on_barrier(barrier_args(&aborted_id, 2, 5_000)).await });
        wait_until_registered(&id, 1).await;

        task.abort();
        assert!(task.await.expect_err("task must be aborted").is_cancelled());
        assert_eq!(registered_waiters(&id), None);

        assert_fresh_pair_rendezvous(&id).await;
    }

    #[tokio::test]
    async fn consecutive_same_id_generations_are_disjoint() {
        let id = unique_barrier_id("generation");
        let first = register_barrier(&barrier_args(&id, 2, 5_000)).expect("first waiter");
        let first_generation = Arc::clone(&first.generation);
        let second = register_barrier(&barrier_args(&id, 2, 5_000)).expect("second waiter");
        assert_eq!(registered_waiters(&id), None);

        let next_first = register_barrier(&barrier_args(&id, 2, 5_000)).expect("next first waiter");
        assert!(!Arc::ptr_eq(&first_generation, &next_first.generation));
        assert_eq!(registered_waiters(&id), Some(1));
        let next_second =
            register_barrier(&barrier_args(&id, 2, 5_000)).expect("next second waiter");
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
