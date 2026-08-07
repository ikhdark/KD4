use super::acquire_named_setup_mutex;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn setup_mutex_waits_until_the_current_owner_releases_it() {
    let mutex_name = format!(
        "Local\\CodexSandboxSetupTest-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    );
    let first_guard = acquire_named_setup_mutex(&mutex_name).expect("acquire first setup mutex");
    let (started_tx, started_rx) = mpsc::channel();
    let (acquired_tx, acquired_rx) = mpsc::channel();

    let waiter = std::thread::spawn(move || {
        started_tx.send(()).expect("signal waiter start");
        let guard = acquire_named_setup_mutex(&mutex_name).expect("acquire setup mutex after wait");
        acquired_tx.send(()).expect("signal setup mutex acquired");
        drop(guard);
    });

    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("waiter started");
    assert_eq!(
        acquired_rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );

    drop(first_guard);
    acquired_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("waiter acquired setup mutex after release");
    waiter.join().expect("join setup mutex waiter");
}
