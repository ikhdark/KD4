use super::*;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::BorrowedHandle;

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
