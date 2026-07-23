use super::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;

#[tokio::test]
async fn tree_shutdowns_start_concurrently_and_preserve_result_order() {
    let shutdown_ids = [ThreadId::new(), ThreadId::new(), ThreadId::new()];
    let barrier = Arc::new(Barrier::new(shutdown_ids.len()));

    let results = tokio::time::timeout(
        Duration::from_secs(1),
        run_tree_shutdowns(&shutdown_ids, |thread_id| {
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
                Ok(thread_id.to_string())
            }
        }),
    )
    .await
    .expect("all shutdown futures should be started together");

    assert_eq!(
        results
            .iter()
            .map(|(thread_id, _)| *thread_id)
            .collect::<Vec<_>>(),
        shutdown_ids
    );
    assert_eq!(
        results
            .into_iter()
            .map(|(_, result)| result.expect("shutdown should succeed"))
            .collect::<Vec<_>>(),
        shutdown_ids.map(|thread_id| thread_id.to_string())
    );
}
