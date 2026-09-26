use anyhow::Context;
use std::fs;
use std::path::Path;
use std::time::Duration;

/// Upper bound on an exit that the native handle reports as soon as it happens.
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn wait_for_pid_file(path: &Path) -> anyhow::Result<String> {
    let pid = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = fs::read_to_string(path) {
                let trimmed = contents.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("timed out waiting for pid file")?;

    Ok(pid)
}

pub async fn process_is_alive(pid: &str) -> anyhow::Result<bool> {
    match native::open(parse_pid(pid)?)? {
        Some(process) => Ok(!process.wait_for_exit(Duration::ZERO)?),
        None => Ok(false),
    }
}

/// Waits on the process's own exit signal instead of polling a process list.
pub async fn wait_for_process_exit(pid: &str) -> anyhow::Result<()> {
    let Some(process) = native::open(parse_pid(pid)?)? else {
        return Ok(());
    };
    let exited = tokio::task::spawn_blocking(move || process.wait_for_exit(PROCESS_EXIT_TIMEOUT))
        .await
        .context("join process exit wait")??;
    anyhow::ensure!(exited, "timed out waiting for process to exit");
    Ok(())
}

fn parse_pid(pid: &str) -> anyhow::Result<u32> {
    pid.parse::<u32>().context("pid file was not numeric")
}

/// Upper bound on reaping a contained root after its process tree is terminated.
const CONTAINED_CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// A fixture child process that owns its whole process tree.
///
/// On Windows the child starts suspended, joins a kill-on-close Job Object, and
/// only then runs, so none of its descendants can start outside the Job.
/// Terminating only the root would leave descendants holding inherited pipes,
/// ports, and the fixture's temporary directories. Dropping this value
/// terminates the tree and waits a bounded time for the root to exit, so an
/// early return or failed assertion cannot strand helper processes.
pub struct ContainedChild {
    child: tokio::process::Child,
    #[cfg(windows)]
    root: codex_utils_pty::ManagedRootProcess,
}

impl ContainedChild {
    /// Spawns `command` inside a new process-tree Job. On Windows this replaces
    /// any creation flags already set on `command`.
    pub fn spawn(command: &mut tokio::process::Command) -> anyhow::Result<Self> {
        command.kill_on_drop(true);
        #[cfg(windows)]
        {
            let root = codex_utils_pty::ManagedRootProcess::reserve()
                .context("reserve a process-tree job")?;
            root.require_descendant_containment()
                .context("keep descendants inside the process-tree job")?;
            command.creation_flags(codex_utils_pty::WINDOWS_CREATE_SUSPENDED);
            let mut child = command.spawn().context("spawn contained child process")?;
            let attached = child
                .id()
                .context("suspended child has no process id")
                .and_then(|pid| {
                    root.attach_and_resume(pid)
                        .context("attach the suspended child to its job and resume it")
                });
            if let Err(error) = attached {
                // The suspended child never ran; terminate it instead of leaking it.
                let _ = child.start_kill();
                return Err(error);
            }
            Ok(Self { child, root })
        }
        #[cfg(not(windows))]
        {
            let child = command.spawn().context("spawn contained child process")?;
            Ok(Self { child })
        }
    }

    /// Terminates the whole process tree and waits for the root to exit.
    pub async fn kill(&mut self) -> std::io::Result<()> {
        self.request_tree_termination();
        self.child.wait().await.map(|_| ())
    }

    fn request_tree_termination(&mut self) {
        #[cfg(windows)]
        let terminated = self.root.terminate().is_ok();
        #[cfg(not(windows))]
        let terminated = false;
        if !terminated {
            let _ = self.child.start_kill();
        }
    }
}

impl std::ops::Deref for ContainedChild {
    type Target = tokio::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl std::ops::DerefMut for ContainedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for ContainedChild {
    fn drop(&mut self) {
        // Drop cannot await. Terminate the tree, then poll until the OS reports
        // the root exited so later teardown, such as removing the fixture's
        // temporary directories, no longer races the process.
        self.request_tree_termination();
        let deadline = std::time::Instant::now() + CONTAINED_CHILD_REAP_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Ok(Some(_)) | Err(_) => return,
            }
        }
    }
}

#[cfg(windows)]
mod native {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    pub(super) struct ProcessHandle(OwnedHandle);

    /// Opens `pid` for exit synchronization; `None` means no such process exists.
    pub(super) fn open(pid: u32) -> anyhow::Result<Option<ProcessHandle>> {
        // SAFETY: OpenProcess has no pointer arguments; a non-null result is a
        // handle this function exclusively owns.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                return Ok(None);
            }
            anyhow::bail!("failed to open process {pid}: {error}");
        }
        // SAFETY: `raw` is a valid, owned process handle returned above.
        Ok(Some(ProcessHandle(unsafe { OwnedHandle::from_raw_handle(raw) })))
    }

    impl ProcessHandle {
        /// Returns whether the process exited within `timeout`.
        pub(super) fn wait_for_exit(&self, timeout: Duration) -> anyhow::Result<bool> {
            // u32::MAX is INFINITE, so saturate just below it.
            let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
            // SAFETY: the handle stays open for the lifetime of `self`.
            match unsafe { WaitForSingleObject(self.0.as_raw_handle(), millis) } {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_TIMEOUT => Ok(false),
                result => anyhow::bail!(
                    "waiting for process exit failed with {result}: {}",
                    io::Error::last_os_error()
                ),
            }
        }
    }
}

#[cfg(not(windows))]
mod native {
    use std::time::Duration;

    pub(super) struct ProcessHandle;

    pub(super) fn open(_pid: u32) -> anyhow::Result<Option<ProcessHandle>> {
        anyhow::bail!("process tracking by pid is only implemented on Windows")
    }

    impl ProcessHandle {
        pub(super) fn wait_for_exit(&self, _timeout: Duration) -> anyhow::Result<bool> {
            anyhow::bail!("process tracking by pid is only implemented on Windows")
        }
    }
}
