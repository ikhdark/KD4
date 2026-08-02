use super::*;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::BorrowedHandle;
use std::time::Duration;

#[test]
fn process_fallback_terminates_root() -> anyhow::Result<()> {
    let mut child = std::process::Command::new("ping.exe")
        .args(["-n", "60", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let process =
        unsafe { BorrowedHandle::borrow_raw(child.as_raw_handle()) }.try_clone_to_owned()?;
    let mut terminator = PipeChildTerminator {
        windows: WindowsChildTerminator::Process(process),
    };

    terminator.kill()?;

    assert!(!child.wait()?.success());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn windows_process_spawn_timeout_does_not_block_async_runtime() {
    let error = run_windows_spawn_operation(Duration::from_millis(20), || {
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    })
    .await
    .expect_err("the blocking spawn operation should time out");

    assert_eq!(error.kind(), ErrorKind::TimedOut);
}
