//! OS exit evidence for test-owned processes, instead of fixed survival sleeps.

use std::io;
use std::time::Duration;

/// Observe a PID recorded by a test's own child. Timeout is a failed observation;
/// cleanup remains with the process owner, never with a potentially reused PID.
pub async fn wait_for_process_exit(pid: u32, timeout: Duration) -> io::Result<()> {
    if pid == 0 || pid == std::process::id() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a test child PID",
        ));
    }
    tokio::task::spawn_blocking(move || wait_for_exit(pid, timeout))
        .await
        .map_err(io::Error::other)?
}

#[cfg(windows)]
fn wait_for_exit(pid: u32, timeout: Duration) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::winbase::WAIT_OBJECT_0;
    use winapi::um::winnt::SYNCHRONIZE;

    // SAFETY: the test supplies its child's PID; the returned handle is owned.
    let raw = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(87) {
            Ok(())
        } else {
            Err(error)
        };
    }
    // SAFETY: OpenProcess returned a valid, uniquely owned handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    let millis = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    // SAFETY: the handle remains live for the bounded wait.
    match unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), millis) } {
        WAIT_OBJECT_0 => Ok(()),
        winapi::shared::winerror::WAIT_TIMEOUT => {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("test child {pid} survived cleanup"),
            ))
        }
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(unix)]
fn wait_for_exit(pid: u32, timeout: Duration) -> io::Result<()> {
    let pid = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid child PID"))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // SAFETY: signal zero only observes this positive test-owned PID.
        if unsafe { libc::kill(pid, 0) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(error);
        }
        // Orphaned Linux children may remain zombies until init reaps them.
        // A zombie has exited and cannot produce any more side effects.
        #[cfg(target_os = "linux")]
        if std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .map(|(_, tail)| tail.trim_start().starts_with('Z'))
            })
            == Some(true)
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("test child {pid} survived cleanup"),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::process::Stdio;

    #[tokio::test]
    async fn observation_rejects_a_live_child_and_accepts_its_exit() -> io::Result<()> {
        #[cfg(windows)]
        let mut command = {
            use std::os::windows::process::CommandExt;
            let mut command = Command::new("cmd.exe");
            command
                .args(["/D", "/S", "/C", "set /p value="])
                .creation_flags(winapi::um::winbase::CREATE_NO_WINDOW);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "read value"]);
            command
        };
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let error = wait_for_process_exit(child.id(), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(child.try_wait()?.is_none(), "observation must not kill the child");
        child.kill()?;
        assert!(!child.wait()?.success());
        wait_for_process_exit(child.id(), Duration::from_secs(1)).await?;
        assert_eq!(
            wait_for_process_exit(0, Duration::ZERO)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        Ok(())
    }
}
