use crate::agent::AgentControl;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Barrier;
use std::thread;

fn control_with_limit(max_threads: usize) -> AgentControl {
    let control = AgentControl::default();
    control.agent_execution_limiter.initialize(max_threads);
    control
}

#[test]
fn execution_guards_count_active_v2_subagent_turns() {
    let control = control_with_limit(/*max_threads*/ 1);
    // Child role configs cannot replace the root-derived session limit.
    control
        .agent_execution_limiter
        .initialize(/*max_threads*/ 2);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));

    let first = control
        .reserve_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("first active turn should fit")
        .expect("v2 subagent execution should be counted");
    let Err(err) = control.reserve_execution_capacity(MultiAgentVersion::V2, &source) else {
        panic!("second active turn should exceed the derived non-root cap");
    };
    let CodexErr::AgentLimitReached { max_threads } = err else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(max_threads, 1);

    drop(first);
    let retry = control
        .reserve_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("capacity check after release")
        .expect("capacity should be released when the running task drops");
    drop(retry);
}

#[test]
fn execution_guards_ignore_root_and_v1_turns() {
    let control = control_with_limit(/*max_threads*/ 0);

    assert!(matches!(
        control.reserve_execution_capacity(MultiAgentVersion::V2, &SessionSource::Cli),
        Ok(None)
    ));
    assert!(matches!(
        control.reserve_execution_capacity(
            MultiAgentVersion::V1,
            &SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()))
        ),
        Ok(None)
    ));
}

#[test]
fn concurrent_execution_reservations_admit_exactly_one() {
    let control = control_with_limit(/*max_threads*/ 1);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));
    let barrier = Arc::new(Barrier::new(2));
    let handles = [ThreadId::new(), ThreadId::new()]
        .into_iter()
        .map(|thread_id| {
            let control = control.clone();
            let source = source.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                match control.reserve_execution_capacity(MultiAgentVersion::V2, &source) {
                    Ok(Some(guard)) => {
                        control
                            .register_execution_permit(thread_id, "turn", Some(guard))?
                            .commit();
                        Ok(thread_id)
                    }
                    Ok(None) => panic!("a V2 subagent admission must acquire a permit"),
                    Err(err) => Err(err),
                }
            })
        })
        .collect::<Vec<_>>();

    let mut admitted = Vec::new();
    let mut rejected = 0;
    for handle in handles {
        match handle.join().expect("execution admission thread panicked") {
            Ok(thread_id) => admitted.push(thread_id),
            Err(CodexErr::AgentLimitReached { max_threads }) => {
                assert_eq!(max_threads, 1);
                rejected += 1;
            }
            Err(err) => panic!("unexpected execution admission error: {err}"),
        }
    }

    assert_eq!(admitted.len(), 1);
    assert_eq!(rejected, 1);
    drop(control.execution_permit_cleanup(admitted.pop().expect("one admitted thread"), "turn"));
    assert!(matches!(
        control.reserve_execution_capacity(MultiAgentVersion::V2, &source),
        Ok(Some(_))
    ));
}

#[test]
fn pending_execution_permit_releases_or_transfers_by_raii() {
    let control = control_with_limit(/*max_threads*/ 1);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));
    let thread_id = ThreadId::new();

    let failed_send_guard = control
        .reserve_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("reserve failed-send permit")
        .expect("V2 subagent should receive a permit");
    let failed_send_registration = control
        .register_execution_permit(thread_id, "failed-send", Some(failed_send_guard))
        .expect("register failed-send permit");
    drop(failed_send_registration);

    let steering_guard = control
        .reserve_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("reserve steering permit")
        .expect("failed-send registration should release its permit");
    control
        .register_execution_permit(thread_id, "steering", Some(steering_guard))
        .expect("register steering permit")
        .commit();
    drop(control.execution_permit_cleanup(thread_id, "steering"));

    let transferred_guard = control
        .reserve_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("reserve transfer permit")
        .expect("steering cleanup should release its permit");
    control
        .register_execution_permit(thread_id, "task", Some(transferred_guard))
        .expect("register task permit")
        .commit();
    let running_task_guard = control
        .execution_guard_for_task(thread_id, "task", MultiAgentVersion::V2, &source)
        .expect("take task permit")
        .expect("registered task permit should transfer");
    assert!(matches!(
        control.reserve_execution_capacity(MultiAgentVersion::V2, &source),
        Err(CodexErr::AgentLimitReached { max_threads: 1 })
    ));
    drop(running_task_guard);
    assert!(matches!(
        control.reserve_execution_capacity(MultiAgentVersion::V2, &source),
        Ok(Some(_))
    ));
}
