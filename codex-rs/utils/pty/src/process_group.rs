//! Process-group helpers shared by pipe/pty and shell command execution.
//!
//! Unix children start in their own process group so cleanup and interrupt
//! signals cover descendants. Other platforms use their native process-tree
//! owner and these helpers are no-ops.

use std::io;

use tokio::process::Child;

/// Observe an owned child's exit while retaining its PID until tree cleanup.
/// The caller must not concurrently wait on or drop the child.
#[cfg(unix)]
pub async fn wait_for_exit_without_reaping(pid: u32) -> io::Result<()> {
    let pid = checked_process_id(pid)?;
    loop {
        // SAFETY: zero is a valid initial siginfo_t representation; waitid writes
        // only to this live value. WNOWAIT leaves the owned child unreaped.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        // SAFETY: successful waitid initializes the child-status fields.
        if unsafe { info.si_pid() } != 0 {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(target_os = "linux")]
pub fn set_parent_death_signal(parent_pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: PR_SET_PDEATHSIG consumes the scalar SIGTERM argument and does not access caller-
    // owned memory.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getppid takes no pointers and has no memory or lifetime preconditions.
    if unsafe { libc::getppid() } != parent_pid {
        // SAFETY: SIGTERM is a valid signal number; raise does not borrow caller-owned memory.
        unsafe { libc::raise(libc::SIGTERM) };
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn set_parent_death_signal(_parent_pid: i32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn detach_from_tty() -> io::Result<()> {
    // SAFETY: setsid takes no arguments and does not access caller-owned memory.
    let result = unsafe { libc::setsid() };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EPERM) {
            return set_process_group();
        }
        return Err(err);
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn detach_from_tty() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn set_process_group() -> io::Result<()> {
    // SAFETY: The zero PID and group select the calling process; setpgid does not access
    // caller-owned memory.
    if unsafe { libc::setpgid(0, 0) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
pub fn set_process_group() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn kill_process_group_by_pid(pid: u32) -> io::Result<()> {
    let pid = checked_process_id(pid)?;
    // SAFETY: getpgid takes only a scalar PID and reports invalid or missing processes through
    // its return value.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid == -1 {
        return ignore_missing_process(io::Error::last_os_error());
    }
    signal_process_group_id(pgid, libc::SIGKILL).map(|_| ())
}

#[cfg(not(unix))]
pub fn kill_process_group_by_pid(_pid: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn ignore_missing_process(error: io::Error) -> io::Result<()> {
    if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn checked_process_id(id: u32) -> io::Result<libc::pid_t> {
    libc::pid_t::try_from(id)
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "process ID must be positive and fit pid_t",
            )
        })
}

#[cfg(unix)]
fn signal_process_group_id(pgid: libc::pid_t, signal: libc::c_int) -> io::Result<bool> {
    // SAFETY: killpg takes scalar group and signal identifiers; OS validation errors are
    // returned to the caller.
    if unsafe { libc::killpg(pgid, signal) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(true)
}

#[cfg(unix)]
pub fn terminate_process_group(process_group_id: u32) -> io::Result<bool> {
    signal_process_group_id(checked_process_id(process_group_id)?, libc::SIGTERM)
}

#[cfg(not(unix))]
pub fn terminate_process_group(_process_group_id: u32) -> io::Result<bool> {
    Ok(false)
}

#[cfg(unix)]
pub fn interrupt_process_group(process_group_id: u32) -> io::Result<()> {
    signal_process_group_id(checked_process_id(process_group_id)?, libc::SIGINT).map(|_| ())
}

#[cfg(not(unix))]
pub fn interrupt_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn kill_process_group(process_group_id: u32) -> io::Result<()> {
    signal_process_group_id(checked_process_id(process_group_id)?, libc::SIGKILL).map(|_| ())
}

#[cfg(not(unix))]
pub fn kill_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn kill_child_process_group(child: &mut Child) -> io::Result<()> {
    if let Some(pid) = child.id() {
        return kill_process_group_by_pid(pid);
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn kill_child_process_group(_child: &mut Child) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::checked_process_id;
    use std::io;

    #[test]
    fn process_id_conversion_rejects_group_selectors_and_overflow() {
        for id in [0, (libc::pid_t::MAX as u32) + 1, u32::MAX] {
            assert_eq!(
                checked_process_id(id)
                    .expect_err("invalid process ID accepted")
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(checked_process_id(1).unwrap(), 1);
        assert_eq!(
            checked_process_id(libc::pid_t::MAX as u32).unwrap(),
            libc::pid_t::MAX
        );
    }
}
