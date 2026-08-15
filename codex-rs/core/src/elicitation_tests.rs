use super::*;

fn lease(owner_id: u64, lease_id: i64) -> OutOfBandElicitationLeaseId {
    OutOfBandElicitationLeaseId::new(owner_id, format!("lease-{lease_id}"))
}

#[tokio::test]
async fn one_out_of_band_lease_blocks_until_release() {
    let service = ElicitationService::new();
    let leases = OutOfBandElicitationLeases::new(service.clone());
    let lease_id = lease(1, 10);
    assert_eq!(leases.acquire(lease_id.clone()).expect("acquire lease"), 1);
    let waiting = tokio::spawn({
        let service = service.clone();
        async move { service.wait_until_clear().await }
    });

    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());

    assert_eq!(leases.release(&lease_id), 0);
    waiting.await.expect("elicitation waiter should complete");
}

#[tokio::test]
async fn multiple_out_of_band_leases_require_every_release() {
    let service = ElicitationService::new();
    let leases = OutOfBandElicitationLeases::new(service.clone());
    let first = lease(1, 10);
    let second = lease(2, 10);
    assert_eq!(leases.acquire(first.clone()).expect("acquire lease"), 1);
    assert_eq!(leases.acquire(second.clone()).expect("acquire lease"), 2);
    let waiting = tokio::spawn({
        let service = service.clone();
        async move { service.wait_until_clear().await }
    });

    assert_eq!(leases.release(&first), 1);
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());

    assert_eq!(leases.release(&second), 0);
    waiting.await.expect("elicitation waiter should complete");
}

#[tokio::test]
async fn explicit_release_is_idempotent_and_cancelled_waiters_do_not_release_leases() {
    let service = ElicitationService::new();
    let leases = OutOfBandElicitationLeases::new(service.clone());
    let lease_id = lease(1, 10);
    let unrelated_lease = lease(2, 20);
    assert_eq!(leases.acquire(lease_id.clone()).expect("acquire lease"), 1);
    assert_eq!(
        leases
            .acquire(unrelated_lease.clone())
            .expect("acquire unrelated lease"),
        2
    );

    let cancelled_waiter = tokio::spawn({
        let service = service.clone();
        async move { service.wait_until_clear().await }
    });
    tokio::task::yield_now().await;
    cancelled_waiter.abort();
    assert!(
        cancelled_waiter
            .await
            .expect_err("waiter should be cancelled")
            .is_cancelled()
    );
    assert_eq!(leases.active_count(), 2);

    let remaining_waiter = tokio::spawn({
        let service = service.clone();
        async move { service.wait_until_clear().await }
    });
    tokio::task::yield_now().await;
    assert!(!remaining_waiter.is_finished());

    assert_eq!(leases.release(&lease_id), 1);
    assert_eq!(leases.release(&lease_id), 1);
    tokio::task::yield_now().await;
    assert!(!remaining_waiter.is_finished());
    assert_eq!(leases.release(&unrelated_lease), 0);
    remaining_waiter
        .await
        .expect("elicitation waiter should complete");
}

#[tokio::test]
async fn closing_out_of_band_leases_clears_every_registration_and_rejects_new_ones() {
    let service = ElicitationService::new();
    let leases = OutOfBandElicitationLeases::new(service.clone());
    assert_eq!(leases.acquire(lease(1, 10)).expect("acquire lease"), 1);
    assert_eq!(leases.acquire(lease(2, 20)).expect("acquire lease"), 2);
    let waiting = tokio::spawn({
        let service = service.clone();
        async move { service.wait_until_clear().await }
    });

    leases.close();
    waiting
        .await
        .expect("closing leases should unblock waiters");
    assert_eq!(leases.active_count(), 0);
    assert!(matches!(
        leases.acquire(lease(3, 30)),
        Err(CodexErr::InvalidRequest(_))
    ));
}
