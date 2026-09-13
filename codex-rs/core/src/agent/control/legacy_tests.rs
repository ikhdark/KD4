use super::*;
use std::sync::Arc;
use tokio::sync::oneshot;

#[tokio::test]
async fn tree_shutdowns_enforce_ceiling_and_restore_input_order() {
    let shutdown_ids = (0..=AGENT_TREE_SHUTDOWN_CONCURRENCY)
        .map(|_| ThreadId::new())
        .collect::<Vec<_>>();
    let mut releases = Vec::new();
    let mut gates = Vec::new();
    for _ in &shutdown_ids {
        let (release, gate) = oneshot::channel();
        releases.push(Some(release));
        gates.push(Some(gate));
    }
    let gates = std::sync::Mutex::new(gates);
    let started = Arc::new(std::sync::Mutex::new(Vec::new()));
    let completed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut shutdowns = Box::pin(run_tree_shutdowns(&shutdown_ids, |thread_id| {
        let index = shutdown_ids.iter().position(|id| *id == thread_id).unwrap();
        let gate = gates.lock().unwrap()[index].take().unwrap();
        let started = Arc::clone(&started);
        let completed = Arc::clone(&completed);
        async move {
            started.lock().unwrap().push(index);
            gate.await.expect("release shutdown");
            completed.lock().unwrap().push(index);
            Ok(thread_id.to_string())
        }
    }));
    assert!(futures::poll!(shutdowns.as_mut()).is_pending());
    assert_eq!(
        *started.lock().unwrap(),
        (0..AGENT_TREE_SHUTDOWN_CONCURRENCY).collect::<Vec<_>>()
    );
    let first_completed = AGENT_TREE_SHUTDOWN_CONCURRENCY - 1;
    releases[first_completed].take().unwrap().send(()).unwrap();
    assert!(futures::poll!(shutdowns.as_mut()).is_pending());
    assert_eq!(started.lock().unwrap().len(), shutdown_ids.len());
    assert_eq!(*completed.lock().unwrap(), vec![first_completed]);
    for index in (1..shutdown_ids.len())
        .rev()
        .filter(|index| *index != first_completed)
    {
        releases[index].take().unwrap().send(()).unwrap();
        assert!(futures::poll!(shutdowns.as_mut()).is_pending());
        assert_eq!(completed.lock().unwrap().last(), Some(&index));
    }
    releases[0].take().unwrap().send(()).unwrap();
    let results = shutdowns.await;
    assert_eq!(
        results
            .into_iter()
            .map(|(id, result)| (id, result.unwrap()))
            .collect::<Vec<_>>(),
        shutdown_ids
            .iter()
            .map(|id| (*id, id.to_string()))
            .collect::<Vec<_>>()
    );
    assert_eq!(completed.lock().unwrap().last(), Some(&0));
}
