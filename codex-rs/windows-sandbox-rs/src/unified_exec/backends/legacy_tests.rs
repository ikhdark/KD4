use super::finalize_exit;
use super::raw_handle;
use super::sendable_handle;
use crate::conpty::ConptyInstance;
use crate::desktop::LaunchDesktop;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;

#[test]
fn wait_thread_state_uses_sendable_handle_storage() {
    fn assert_send<T: Send>() {}

    assert_send::<LaunchDesktop>();
    assert_send::<ConptyInstance>();
    assert_send::<Arc<Mutex<Option<super::SendableHandle>>>>();

    let address = 0x1234usize;
    assert_eq!(sendable_handle(raw_handle(address)), address);
}

#[test]
fn final_wait_failure_does_not_join_output_reader() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let output_join = std::thread::spawn(move || {
        let _ = release_rx.recv();
    });
    let (exit_tx, mut exit_rx) = oneshot::channel();
    let process_handle = Arc::new(Mutex::new(Some(/*invalid process handle*/ 0)));

    let started = Instant::now();
    finalize_exit(
        exit_tx,
        process_handle,
        /*thread_handle*/ 0,
        output_join,
        /*logs_base_dir*/ None,
        vec!["test-command".to_string()],
        /*termination_requested*/ true,
    );

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "invalid final wait blocked on the output reader"
    );
    assert_eq!(exit_rx.try_recv(), Ok(1));
    drop(release_tx);
}
