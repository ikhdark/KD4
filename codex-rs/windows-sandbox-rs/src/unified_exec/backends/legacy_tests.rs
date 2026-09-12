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

#[test]
fn native_write_request_length_preserves_the_dword_boundary() {
    assert_eq!(super::native_write_request_len(0), 0);
    assert_eq!(super::native_write_request_len(23), 23);
    assert_eq!(super::native_write_request_len(u32::MAX as usize), u32::MAX);
    if let Some(oversized) = (u32::MAX as usize).checked_add(1) {
        assert_eq!(super::native_write_request_len(oversized), u32::MAX);
        assert_eq!(super::native_write_request_len(usize::MAX), u32::MAX);
    }
}

#[tokio::test]
async fn native_input_writer_delivers_all_bytes_and_closes_the_pipe() -> anyhow::Result<()> {
    use std::io::Read;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::IntoRawHandle;
    use std::os::windows::io::OwnedHandle;
    use windows_sys::Win32::System::Pipes::CreatePipe;

    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    // SAFETY: both output pointers are valid; successful handles immediately
    // enter RAII owners, then the production writer takes sole write ownership.
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null_mut(), 0) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut reader = unsafe { std::fs::File::from_raw_handle(read.cast()) };
    let writer = unsafe { OwnedHandle::from_raw_handle(write.cast()) };
    let reader = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map(|_| bytes)
    });
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let writer = super::spawn_input_writer(Some(writer.into_raw_handle().cast()), receiver, false);
    let payload = (0..131_072)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    sender.send(payload.clone()).await?;
    sender.send(b"tail".to_vec()).await?;
    drop(sender);
    tokio::time::timeout(Duration::from_secs(5), writer).await??;
    let received = tokio::time::timeout(Duration::from_secs(5), reader).await???;
    let mut expected = payload;
    expected.extend_from_slice(b"tail");
    assert_eq!(received, expected);
    Ok(())
}
