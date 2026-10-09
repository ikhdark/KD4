use super::*;

fn lease(owner_id: u64, lease_id: i64) -> OutOfBandElicitationLeaseId {
    OutOfBandElicitationLeaseId::new(owner_id, format!("lease-{lease_id}"))
}

#[tokio::test]
async fn out_of_band_leases_require_every_release() {
    for lease_count in [1, 2] {
        let service = ElicitationService::new();
        let leases = OutOfBandElicitationLeases::new(service.clone());
        for owner in 1..=lease_count {
            assert_eq!(
                leases.acquire(lease(owner, 10)).expect("acquire lease"),
                owner as i64
            );
        }
        let mut waiting = Box::pin(service.wait_until_clear());
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        for owner in 1..=lease_count {
            assert_eq!(
                leases.release(&lease(owner, 10)),
                (lease_count - owner) as i64
            );
            if owner < lease_count {
                // Every registration must finish before delivery can resume. Poll
                // the waiter itself so a prematurely cleared pause cannot hide
                // behind a spawned task that has not been scheduled yet.
                assert!(futures::poll!(waiting.as_mut()).is_pending());
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("elicitation waiter should complete");
    }
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
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !service.has_waiters_for_test() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled waiter must subscribe before cancellation");
    cancelled_waiter.abort();
    assert!(
        cancelled_waiter
            .await
            .expect_err("waiter should be cancelled")
            .is_cancelled()
    );
    assert_eq!(leases.active_count(), 2);

    let mut remaining_waiter = Box::pin(service.wait_until_clear());
    assert!(futures::poll!(remaining_waiter.as_mut()).is_pending());

    assert_eq!(leases.release(&lease_id), 1);
    assert_eq!(leases.release(&lease_id), 1);
    assert!(futures::poll!(remaining_waiter.as_mut()).is_pending());
    assert_eq!(leases.release(&unrelated_lease), 0);
    tokio::time::timeout(std::time::Duration::from_secs(1), remaining_waiter)
        .await
        .expect("elicitation waiter should complete");
}

#[tokio::test]
async fn closing_out_of_band_leases_clears_every_registration_and_rejects_new_ones() {
    let service = ElicitationService::new();
    let leases = OutOfBandElicitationLeases::new(service.clone());
    assert_eq!(leases.acquire(lease(1, 10)).expect("acquire lease"), 1);
    assert_eq!(leases.acquire(lease(2, 20)).expect("acquire lease"), 2);
    let mut waiting = Box::pin(service.wait_until_clear());
    assert!(futures::poll!(waiting.as_mut()).is_pending());

    leases.close();
    tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
        .await
        .expect("closing leases should unblock waiters");
    assert_eq!(leases.active_count(), 0);
    assert!(matches!(
        leases.acquire(lease(3, 30)),
        Err(CodexErr::InvalidRequest(_))
    ));
}
