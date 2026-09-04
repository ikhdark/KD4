use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(windows)]
use std::collections::BTreeMap;
#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use std::path::Path;

const MANAGED_ROOT_WARNING_THRESHOLD: usize = 384;
const MANAGED_ROOT_LIMIT: usize = 512;
const MANAGED_ROOT_RECLAIM_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound for synchronous Windows process operations moved off Tokio's
/// async worker threads.
#[cfg(windows)]
pub const WINDOWS_PROCESS_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Creation flag used when Job membership must be established before any
/// child code is allowed to run.
#[cfg(windows)]
pub const WINDOWS_CREATE_SUSPENDED: u32 = winapi::um::winbase::CREATE_SUSPENDED;

static MANAGED_ROOT_COUNT: AtomicUsize = AtomicUsize::new(0);
static NEXT_MANAGED_ROOT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_RECLAIMER_ID: AtomicU64 = AtomicU64::new(1);
static ADMISSION_RECLAIMERS: OnceLock<Mutex<Vec<(u64, AdmissionReclaimer)>>> = OnceLock::new();
static ADMISSION_RECLAIM_PERMIT: OnceLock<tokio::sync::Semaphore> = OnceLock::new();

pub type ManagedRootReclaimFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
pub type ManagedRootReclaimHook = Arc<dyn Fn() -> ManagedRootReclaimFuture + Send + Sync>;

#[derive(Clone)]
struct AdmissionReclaimer {
    retire_zero_lease_mcp_generations: ManagedRootReclaimHook,
    evict_one_eligible_task: ManagedRootReclaimHook,
}

pub struct ManagedRootAdmissionReclaimerGuard {
    id: u64,
}

impl Drop for ManagedRootAdmissionReclaimerGuard {
    fn drop(&mut self) {
        admission_reclaimers()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(id, _)| *id != self.id);
    }
}

pub fn install_managed_root_admission_reclaimer(
    retire_zero_lease_mcp_generations: ManagedRootReclaimHook,
    evict_one_eligible_task: ManagedRootReclaimHook,
) -> ManagedRootAdmissionReclaimerGuard {
    let id = NEXT_RECLAIMER_ID.fetch_add(1, Ordering::Relaxed);
    admission_reclaimers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((
            id,
            AdmissionReclaimer {
                retire_zero_lease_mcp_generations,
                evict_one_eligible_task,
            },
        ));
    ManagedRootAdmissionReclaimerGuard { id }
}

fn admission_reclaimers() -> &'static Mutex<Vec<(u64, AdmissionReclaimer)>> {
    ADMISSION_RECLAIMERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Admission and containment owner for one managed root process tree.
///
/// Keep this value alive until the root has been reaped. On Windows its Job
/// Object also owns every descendant in the tree.
pub struct ManagedRootProcess {
    admission: Arc<ManagedRootAdmissionLease>,
    #[cfg(windows)]
    job: Arc<crate::win::JobObject>,
}

struct ManagedRootAdmissionLease {
    id: u64,
}

#[cfg(windows)]
pub struct WindowsManagedChild {
    _admission: Arc<ManagedRootAdmissionLease>,
    job: Arc<crate::win::JobObject>,
    process: filedescriptor::OwnedHandle,
    primary_thread: Option<filedescriptor::OwnedHandle>,
    pid: u32,
    stdin: Option<std::fs::File>,
    stdout: Option<std::fs::File>,
    stderr: Option<std::fs::File>,
}

#[cfg(windows)]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsSuspendedSpawnFault {
    None,
    InheritanceReset,
    FileDescriptorConversion,
}

/// A cloneable termination handle for a Windows managed root's Job Object.
///
/// Cloning this value does not reserve another managed-root slot.
#[cfg(windows)]
#[derive(Clone)]
pub struct WindowsJobControl {
    job: Arc<crate::win::JobObject>,
}

impl ManagedRootProcess {
    /// Reserve one of the process-wide managed-root slots before spawning.
    pub fn reserve() -> io::Result<Self> {
        Self::reserve_with_breakaway(/*allow_breakaway*/ true)
    }

    fn reserve_with_breakaway(allow_breakaway: bool) -> io::Result<Self> {
        let mut current = MANAGED_ROOT_COUNT.load(Ordering::Acquire);
        loop {
            if current >= MANAGED_ROOT_LIMIT {
                return Err(io::Error::other(format!(
                    "managed root process limit reached ({MANAGED_ROOT_LIMIT})"
                )));
            }
            match MANAGED_ROOT_COUNT.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }

        let count = current + 1;
        if count == MANAGED_ROOT_WARNING_THRESHOLD {
            log::warn!(
                "managed root process count reached warning threshold ({count}/{MANAGED_ROOT_LIMIT})"
            );
        }

        #[cfg(windows)]
        let job = match if allow_breakaway {
            crate::win::JobObject::create()
        } else {
            crate::win::JobObject::create_strict()
        } {
            Ok(job) => Arc::new(job),
            Err(error) => {
                MANAGED_ROOT_COUNT.fetch_sub(1, Ordering::AcqRel);
                return Err(error);
            }
        };

        Ok(Self {
            admission: Arc::new(ManagedRootAdmissionLease {
                id: NEXT_MANAGED_ROOT_ID.fetch_add(1, Ordering::Relaxed),
            }),
            #[cfg(windows)]
            job,
        })
    }

    /// Reserve a root, attempting one deadline-bounded serialized cross-layer
    /// reclaim pass before rejecting a launch at the hard limit.
    pub async fn reserve_with_reclaim() -> io::Result<Self> {
        Self::reserve_with_reclaim_policy(
            MANAGED_ROOT_RECLAIM_TIMEOUT,
            /*allow_breakaway*/ true,
        )
        .await
    }

    /// Reserve a managed root whose Windows Job forbids explicit descendant breakaway.
    pub async fn reserve_strict_with_reclaim() -> io::Result<Self> {
        Self::reserve_with_reclaim_policy(
            MANAGED_ROOT_RECLAIM_TIMEOUT,
            /*allow_breakaway*/ false,
        )
        .await
    }

    async fn reserve_with_reclaim_policy(
        reclaim_timeout: Duration,
        allow_breakaway: bool,
    ) -> io::Result<Self> {
        match Self::reserve_with_breakaway(allow_breakaway) {
            Ok(root) => return Ok(root),
            Err(error) if MANAGED_ROOT_COUNT.load(Ordering::Acquire) < MANAGED_ROOT_LIMIT => {
                return Err(error);
            }
            Err(_) => {}
        }

        let reclaim = async {
            let _reclaim = ADMISSION_RECLAIM_PERMIT
                .get_or_init(|| tokio::sync::Semaphore::new(1))
                .acquire()
                .await
                .map_err(|error| {
                    io::Error::other(format!("admission reclaimer closed: {error}"))
                })?;
            if let Ok(root) = Self::reserve_with_breakaway(allow_breakaway) {
                return Ok(root);
            }

            // Root finalizers release permits on drop. Drain already-ready drops
            // before asking the runtime and app-server owners to reclaim.
            tokio::task::yield_now().await;

            let reclaimers = admission_reclaimers()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .rev()
                .map(|(_, reclaimer)| reclaimer.clone())
                .collect::<Vec<_>>();
            for reclaimer in &reclaimers {
                (reclaimer.retire_zero_lease_mcp_generations)().await;
            }
            if let Ok(root) = Self::reserve_with_breakaway(allow_breakaway) {
                return Ok(root);
            }
            for reclaimer in reclaimers {
                (reclaimer.evict_one_eligible_task)().await;
                if let Ok(root) = Self::reserve_with_breakaway(allow_breakaway) {
                    return Ok(root);
                }
            }

            Self::reserve_with_breakaway(allow_breakaway)
        };

        tokio::time::timeout(reclaim_timeout, reclaim)
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "managed root process reclamation timed out after {} ms at the hard limit ({MANAGED_ROOT_LIMIT})",
                        reclaim_timeout.as_millis()
                    ),
                )
            })?
    }

    pub fn id(&self) -> u64 {
        self.admission.id
    }

    /// Open a normally running Windows root by PID and attach it to the Job.
    #[cfg(windows)]
    pub fn attach(&self, pid: u32) -> io::Result<()> {
        use std::os::windows::io::FromRawHandle;
        use std::os::windows::io::OwnedHandle;
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::winnt::PROCESS_SET_QUOTA;
        use winapi::um::winnt::PROCESS_TERMINATE;

        let raw = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let _process = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
        self.job.assign_process(raw.cast())
    }

    /// Attach a `CREATE_SUSPENDED` child to the Job and then resume all of its
    /// threads. Enumerating the new process's threads recovers the primary
    /// thread handle that `std::process` and Tokio do not expose.
    #[cfg(windows)]
    pub fn attach_and_resume(&self, pid: u32) -> io::Result<()> {
        self.attach(pid)?;
        resume_process_threads(pid)
    }

    #[cfg(windows)]
    pub fn terminate(&self) -> io::Result<()> {
        self.job.terminate()
    }

    /// Returns the number of processes that remain active in the managed Job.
    #[cfg(windows)]
    pub fn active_process_count(&self) -> io::Result<u32> {
        self.job.active_process_count()
    }

    #[cfg(windows)]
    pub fn windows_job_control(&self) -> WindowsJobControl {
        WindowsJobControl {
            job: Arc::clone(&self.job),
        }
    }

    #[cfg(windows)]
    pub fn preserve_descendants(&self) -> io::Result<()> {
        self.job.preserve_descendants()
    }

    /// Consumes this reservation while the blocking Windows loader creates a
    /// suspended root atomically assigned to its strict Job.
    #[cfg(windows)]
    pub fn spawn_windows_suspended(
        self,
        executable: &OsStr,
        args: &[OsString],
        cwd: &Path,
        env: &BTreeMap<OsString, OsString>,
    ) -> io::Result<(Self, WindowsManagedChild)> {
        self.spawn_windows_suspended_with_fault(
            executable,
            args,
            cwd,
            env,
            WindowsSuspendedSpawnFault::None,
        )
    }

    #[doc(hidden)]
    #[cfg(windows)]
    pub fn spawn_windows_suspended_with_fault(
        self,
        executable: &OsStr,
        args: &[OsString],
        cwd: &Path,
        env: &BTreeMap<OsString, OsString>,
        fault: WindowsSuspendedSpawnFault,
    ) -> io::Result<(Self, WindowsManagedChild)> {
        let created = self.job.create_suspended_with_pipes(
            executable,
            args,
            cwd,
            env,
            fault == WindowsSuspendedSpawnFault::InheritanceReset,
        )?;
        let converted = (|| {
            if fault == WindowsSuspendedSpawnFault::FileDescriptorConversion {
                return Err(io::Error::other(
                    "injected post-CreateProcessW file-descriptor conversion failure",
                ));
            }
            Ok((
                created.stdin.as_file().map_err(io::Error::other)?,
                created.stdout.as_file().map_err(io::Error::other)?,
                created.stderr.as_file().map_err(io::Error::other)?,
            ))
        })();
        let (stdin, stdout, stderr) = match converted {
            Ok(converted) => converted,
            Err(error) => {
                return Err(self
                    .job
                    .cleanup_created_process_error(&created.process, error));
            }
        };
        let child = WindowsManagedChild {
            _admission: Arc::clone(&self.admission),
            job: Arc::clone(&self.job),
            process: created.process,
            primary_thread: Some(created.primary_thread),
            pid: created.pid,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
        };
        Ok((self, child))
    }
}

#[cfg(windows)]
impl WindowsJobControl {
    pub fn terminate(&self) -> io::Result<()> {
        self.job.terminate()
    }

    pub fn active_process_count(&self) -> io::Result<u32> {
        self.job.active_process_count()
    }
}

#[cfg(windows)]
impl WindowsManagedChild {
    pub fn id(&self) -> Option<u32> {
        Some(self.pid)
    }

    pub fn take_stdin(&mut self) -> Option<std::fs::File> {
        self.stdin.take()
    }

    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    pub fn take_stdout(&mut self) -> Option<std::fs::File> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<std::fs::File> {
        self.stderr.take()
    }

    /// Resumes only the primary thread returned by CreateProcessW.
    pub fn resume_primary_thread(&mut self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use winapi::um::processthreadsapi::ResumeThread;

        let thread = self.primary_thread.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "primary thread already resumed",
            )
        })?;
        if unsafe { ResumeThread(thread.as_raw_handle().cast()) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn start_kill(&mut self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use winapi::um::processthreadsapi::TerminateProcess;

        match self.job.terminate() {
            Ok(()) => Ok(()),
            Err(job_error) => {
                if unsafe { TerminateProcess(self.process.as_raw_handle().cast(), 1) } == 0 {
                    Err(io::Error::other(format!(
                        "failed to terminate Windows Job ({job_error}); root fallback also failed: {}",
                        io::Error::last_os_error()
                    )))
                } else {
                    Ok(())
                }
            }
        }
    }

    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        let process = self.process.try_clone().map_err(io::Error::other)?;
        tokio::task::spawn_blocking(move || wait_for_windows_process(process, None))
            .await
            .map_err(|error| io::Error::other(format!("Windows wait task failed: {error}")))?
    }

    pub fn wait_blocking(&mut self) -> io::Result<std::process::ExitStatus> {
        let process = self.process.try_clone().map_err(io::Error::other)?;
        wait_for_windows_process(process, None)
    }

    pub fn wait_blocking_until(
        &mut self,
        deadline: std::time::Instant,
    ) -> io::Result<std::process::ExitStatus> {
        let process = self.process.try_clone().map_err(io::Error::other)?;
        wait_for_windows_process(process, Some(deadline))
    }
}

#[cfg(windows)]
fn wait_for_windows_process(
    process: filedescriptor::OwnedHandle,
    deadline: Option<std::time::Instant>,
) -> io::Result<std::process::ExitStatus> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::ExitStatusExt;
    use winapi::shared::minwindef::DWORD;
    use winapi::um::processthreadsapi::GetExitCodeProcess;
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::winbase::INFINITE;

    const WAIT_TIMEOUT_RESULT: u32 = 0x0000_0102;

    let timeout = deadline.map_or(INFINITE, |deadline| {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        u32::try_from(remaining.as_millis().max(1)).unwrap_or(u32::MAX - 1)
    });
    let waited = unsafe { WaitForSingleObject(process.as_raw_handle().cast(), timeout) };
    if waited == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    if waited == WAIT_TIMEOUT_RESULT {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Windows process wait deadline expired",
        ));
    }
    let mut code: DWORD = 0;
    if unsafe { GetExitCodeProcess(process.as_raw_handle().cast(), &mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(std::process::ExitStatus::from_raw(code))
}

#[cfg(windows)]
impl Drop for WindowsManagedChild {
    fn drop(&mut self) {
        if let Err(error) = self.job.terminate_reap_and_observe_empty(&self.process) {
            log::warn!("failed to terminate and reap dropped Windows managed child: {error}");
            // Fail closed: an unconfirmed process-tree teardown must continue
            // consuming its admission slot.
            std::mem::forget(Arc::clone(&self._admission));
        }
    }
}

/// Run a synchronous Windows process operation on Tokio's blocking pool with
/// a bounded wait so a stuck loader cannot stall an async worker thread.
#[cfg(windows)]
pub async fn run_windows_process_operation<T, F>(timeout: Duration, operation: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    tokio::time::timeout(timeout, tokio::task::spawn_blocking(operation))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "Windows process operation did not return within {} seconds",
                    timeout.as_secs_f64()
                ),
            )
        })?
        .map_err(|error| io::Error::other(format!("Windows process task failed: {error}")))?
}

#[cfg(windows)]
fn resume_process_threads(pid: u32) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::processthreadsapi::OpenThread;
    use winapi::um::processthreadsapi::ResumeThread;
    use winapi::um::tlhelp32::CreateToolhelp32Snapshot;
    use winapi::um::tlhelp32::TH32CS_SNAPTHREAD;
    use winapi::um::tlhelp32::THREADENTRY32;
    use winapi::um::tlhelp32::Thread32First;
    use winapi::um::tlhelp32::Thread32Next;
    use winapi::um::winnt::THREAD_SUSPEND_RESUME;

    let raw_snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw_snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw_snapshot.cast()) };
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut has_entry = unsafe { Thread32First(snapshot.as_raw_handle().cast(), &mut entry) } != 0;
    let mut resumed = 0usize;

    while has_entry {
        if entry.th32OwnerProcessID == pid {
            let raw_thread =
                unsafe { OpenThread(THREAD_SUSPEND_RESUME, FALSE, entry.th32ThreadID) };
            if raw_thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(raw_thread.cast()) };
            if unsafe { ResumeThread(thread.as_raw_handle().cast()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            resumed += 1;
        }
        has_entry = unsafe { Thread32Next(snapshot.as_raw_handle().cast(), &mut entry) } != 0;
    }

    if resumed == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no thread found for suspended process {pid}"),
        ));
    }
    Ok(())
}

impl Drop for ManagedRootAdmissionLease {
    fn drop(&mut self) {
        let remaining = MANAGED_ROOT_COUNT.fetch_sub(1, Ordering::AcqRel) - 1;
        log::debug!(
            "released managed root process lifecycle_id={} remaining={remaining}",
            self.id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    const STRICT_HELPER_MODE: &str = "KD4_PTY_STRICT_HELPER_MODE";
    #[cfg(windows)]
    const STRICT_HELPER_MARKER: &str = "KD4_PTY_STRICT_HELPER_MARKER";
    #[cfg(windows)]
    const STRICT_HELPER_HANDLE: &str = "KD4_PTY_STRICT_HELPER_HANDLE";
    #[cfg(windows)]
    const STRICT_HELPER_REVERSE_HANDLES: &str = "KD4_PTY_STRICT_HELPER_REVERSE_HANDLES";
    #[cfg(windows)]
    const STRICT_REVERSE_TEST_HELPER: &str = "KD4_PTY_STRICT_REVERSE_TEST_HELPER";
    #[cfg(windows)]
    const STRICT_RESET_FAILURE_TEST_HELPER: &str = "KD4_PTY_STRICT_RESET_FAILURE_TEST_HELPER";
    #[cfg(windows)]
    const MANAGED_ROOT_ADMISSION_TEST_HELPER: &str = "KD4_PTY_MANAGED_ROOT_ADMISSION_TEST_HELPER";
    #[cfg(windows)]
    const STRICT_HELPER_UNICODE_ENV: &str = "Kd4_Ünicode_Env";
    #[cfg(windows)]
    const STRICT_HELPER_EMPTY_ENV: &str = "KD4_EMPTY_ENV";

    fn managed_root_test_lock() -> &'static tokio::sync::Semaphore {
        static TEST_LOCK: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
        TEST_LOCK.get_or_init(|| tokio::sync::Semaphore::new(1))
    }

    #[cfg(windows)]
    fn run_managed_root_test_isolated(test_name: &str) -> io::Result<bool> {
        if std::env::var_os(MANAGED_ROOT_ADMISSION_TEST_HELPER).is_some() {
            return Ok(false);
        }
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env(MANAGED_ROOT_ADMISSION_TEST_HELPER, "1");
        let mut child = crate::with_windows_child_creation(|_| command.spawn())?;
        let status = child.wait()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "isolated managed-root test {test_name} failed: {status}"
            )));
        }
        Ok(true)
    }

    #[cfg(windows)]
    fn strict_helper_command(
        mode: &str,
        marker: &std::path::Path,
    ) -> io::Result<(
        std::path::PathBuf,
        Vec<OsString>,
        BTreeMap<OsString, OsString>,
    )> {
        let executable = std::env::current_exe()?;
        let args = vec![
            OsString::from("--exact"),
            OsString::from("managed_process::tests::strict_containment_process_helper"),
            OsString::from("--nocapture"),
        ];
        let mut env = std::env::vars_os().collect::<BTreeMap<_, _>>();
        env.insert(OsString::from(STRICT_HELPER_MODE), OsString::from(mode));
        env.insert(
            OsString::from(STRICT_HELPER_MARKER),
            marker.as_os_str().to_os_string(),
        );
        Ok((executable, args, env))
    }

    #[cfg(windows)]
    fn unique_strict_helper_marker(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kd4-pty-strict-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos()
        ))
    }

    #[cfg(windows)]
    fn wait_for_marker(marker: &std::path::Path) -> io::Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "strict containment helper did not create {}",
                        marker.display()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    #[cfg(windows)]
    fn reserve_noninheritable_event_handles(
        count: usize,
    ) -> io::Result<Vec<std::os::windows::io::OwnedHandle>> {
        use std::os::windows::io::FromRawHandle;
        use winapi::um::synchapi::CreateEventW;

        let mut handles = Vec::with_capacity(count);
        for _ in 0..count {
            let handle = unsafe {
                CreateEventW(
                    std::ptr::null_mut(),
                    /*manual_reset*/ 0,
                    /*initial_state*/ 0,
                    std::ptr::null(),
                )
            };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            handles
                .push(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle.cast()) });
        }
        Ok(handles)
    }

    /// Runs only inside a child test process launched by the strict-spawn tests.
    #[cfg(windows)]
    #[test]
    fn strict_containment_process_helper() {
        use std::io::Read;
        use std::io::Write;
        use std::os::windows::ffi::OsStrExt;

        let Some(mode) = std::env::var_os(STRICT_HELPER_MODE) else {
            return;
        };
        let marker = std::env::var_os(STRICT_HELPER_MARKER)
            .map(std::path::PathBuf::from)
            .expect("strict helper marker is present");
        match mode.to_string_lossy().as_ref() {
            "stdio" => {
                std::fs::write(&marker, b"started").expect("write strict helper marker");
                let mut input = String::new();
                std::io::stdin()
                    .read_to_string(&mut input)
                    .expect("read strict helper stdin");
                std::io::stdout()
                    .write_all(format!("strict-echo:{input}").as_bytes())
                    .expect("write strict helper stdout");
            }
            "descendant-root" => {
                let child = std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("--exact")
                    .arg("managed_process::tests::strict_containment_process_helper")
                    .arg("--nocapture")
                    .env(STRICT_HELPER_MODE, "descendant")
                    .env(STRICT_HELPER_MARKER, &marker)
                    .spawn()
                    .expect("spawn strict helper descendant");
                std::fs::write(&marker, child.id().to_string())
                    .expect("write strict helper descendant marker");
                drop(child);
                std::thread::sleep(Duration::from_secs(60));
            }
            "descendant" => std::thread::sleep(Duration::from_secs(60)),
            "handle-probe" => {
                use winapi::um::synchapi::SetEvent;

                let handle = std::env::var(STRICT_HELPER_HANDLE)
                    .expect("strict helper handle is present")
                    .parse::<usize>()
                    .expect("strict helper handle is numeric");
                let signaled = unsafe { SetEvent(handle as *mut winapi::ctypes::c_void) };
                std::fs::write(&marker, signaled.to_string())
                    .expect("write strict handle-probe marker");
            }
            "reverse-handle-probe" => {
                use winapi::um::handleapi::CloseHandle;
                use winapi::um::handleapi::GetHandleInformation;
                use winapi::um::processenv::GetStdHandle;
                use winapi::um::winbase::STD_ERROR_HANDLE;
                use winapi::um::winbase::STD_INPUT_HANDLE;
                use winapi::um::winbase::STD_OUTPUT_HANDLE;

                let mut closed = std::collections::BTreeSet::new();
                for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                    let handle = unsafe { GetStdHandle(kind) };
                    if !handle.is_null() && closed.insert(handle as usize) {
                        unsafe { CloseHandle(handle) };
                    }
                }

                let validity = std::env::var(STRICT_HELPER_REVERSE_HANDLES)
                    .expect("reverse handle list is present")
                    .split(',')
                    .map(|value| {
                        let handle = value
                            .parse::<usize>()
                            .expect("reverse handle candidate is numeric");
                        let mut flags = 0;
                        usize::from(
                            unsafe {
                                GetHandleInformation(
                                    handle as *mut winapi::ctypes::c_void,
                                    &mut flags,
                                )
                            } != 0,
                        )
                    })
                    .collect::<Vec<_>>();
                std::fs::write(
                    &marker,
                    validity
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                )
                .expect("write reverse handle-probe marker");
            }
            "environment-probe" => {
                let unicode = std::env::var("kD4_ünICODE_eNV")
                    .expect("Unicode environment value is present through case-insensitive lookup");
                let empty = std::env::var_os("kd4_empty_env")
                    .expect("empty environment value is present through case-insensitive lookup");
                std::fs::write(
                    &marker,
                    format!("{unicode}\n{}", empty.as_os_str().encode_wide().count()),
                )
                .expect("write strict environment-probe marker");
            }
            "argv-probe" => {
                let mut after_separator = false;
                let mut encoded = Vec::new();
                for value in std::env::args_os() {
                    if !after_separator {
                        after_separator = value == "--";
                        continue;
                    }
                    let units = value.encode_wide().collect::<Vec<_>>();
                    let mut line = units.len().to_string();
                    for unit in units {
                        line.push(',');
                        line.push_str(&unit.to_string());
                    }
                    encoded.push(line);
                }
                std::fs::write(&marker, encoded.join("\n"))
                    .expect("write strict argv-probe marker");
            }
            "breakaway-probe" => {
                use std::os::windows::process::CommandExt;

                const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
                let executable = std::env::var_os("ComSpec")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));
                let result = std::process::Command::new(executable)
                    .args(["/d", "/c", "exit 0"])
                    .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
                    .spawn();
                let outcome = match result {
                    Ok(mut child) => {
                        let pid = child.id();
                        let _ = child.kill();
                        let _ = child.wait();
                        format!("spawned:{pid}")
                    }
                    Err(error) => format!("error:{}", error.raw_os_error().unwrap_or_default()),
                };
                std::fs::write(&marker, outcome).expect("write strict breakaway-probe marker");
            }
            other => panic!("unknown strict helper mode {other}"),
        }
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn strict_spawn_stays_suspended_then_round_trips_real_stdio() -> io::Result<()> {
        use std::io::Read;
        use std::io::Write;

        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let marker = unique_strict_helper_marker("stdio");
        let _ = std::fs::remove_file(&marker);
        let (executable, args, env) = strict_helper_command("stdio", &marker)?;
        let managed = ManagedRootProcess::reserve_strict_with_reclaim().await?;
        let preserve_error = managed
            .preserve_descendants()
            .expect_err("strict managed root must not relax containment");
        assert_eq!(preserve_error.kind(), io::ErrorKind::PermissionDenied);
        let (managed, mut child) = managed.spawn_windows_suspended(
            executable.as_os_str(),
            &args,
            &std::env::current_dir()?,
            &env,
        )?;
        assert_eq!(managed.active_process_count()?, 1);
        assert!(!marker.exists(), "suspended child must not execute");

        let mut stdin = child.take_stdin().expect("strict child stdin");
        let mut stdout = child.take_stdout().expect("strict child stdout");
        child.resume_primary_thread()?;
        wait_for_marker(&marker)?;
        stdin.write_all(b"runtime-path\n")?;
        drop(stdin);
        let status =
            child.wait_blocking_until(std::time::Instant::now() + Duration::from_secs(10))?;
        assert!(status.success());
        let mut output = String::new();
        stdout.read_to_string(&mut output)?;
        assert!(output.contains("strict-echo:runtime-path"), "{output:?}");

        drop(child);
        drop(managed);
        std::fs::remove_file(marker)?;
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn strict_spawn_inherits_only_its_explicit_handle_list() -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::io::FromRawHandle;
        use std::os::windows::io::OwnedHandle;
        use winapi::shared::minwindef::FALSE;
        use winapi::shared::minwindef::TRUE;
        use winapi::um::handleapi::SetHandleInformation;
        use winapi::um::synchapi::CreateEventW;
        use winapi::um::synchapi::WaitForSingleObject;
        use winapi::um::winbase::HANDLE_FLAG_INHERIT;

        const WAIT_TIMEOUT_RESULT: u32 = 0x0000_0102;

        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let marker = unique_strict_helper_marker("handle-list");
        let _ = std::fs::remove_file(&marker);
        let (executable, args, mut env) = strict_helper_command("handle-probe", &marker)?;
        let raw_event =
            unsafe { CreateEventW(std::ptr::null_mut(), TRUE, FALSE, std::ptr::null()) };
        if raw_event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = unsafe { OwnedHandle::from_raw_handle(raw_event.cast()) };
        if unsafe {
            SetHandleInformation(
                event.as_raw_handle().cast(),
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        env.insert(
            OsString::from(STRICT_HELPER_HANDLE),
            OsString::from((event.as_raw_handle() as usize).to_string()),
        );

        let managed = ManagedRootProcess::reserve_strict_with_reclaim().await?;
        let spawned = managed.spawn_windows_suspended(
            executable.as_os_str(),
            &args,
            &std::env::current_dir()?,
            &env,
        );
        if unsafe { SetHandleInformation(event.as_raw_handle().cast(), HANDLE_FLAG_INHERIT, 0) }
            == 0
        {
            drop(spawned);
            return Err(io::Error::last_os_error());
        }
        let (managed, mut child) = spawned?;
        child.resume_primary_thread()?;
        let status =
            child.wait_blocking_until(std::time::Instant::now() + Duration::from_secs(10))?;
        assert!(status.success());
        wait_for_marker(&marker)?;
        assert_eq!(std::fs::read_to_string(&marker)?, "0");
        assert_eq!(
            unsafe { WaitForSingleObject(event.as_raw_handle().cast(), 0) },
            WAIT_TIMEOUT_RESULT,
            "an inheritable handle outside the explicit allow-list reached the child"
        );

        drop(child);
        drop(managed);
        std::fs::remove_file(marker)?;
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn public_pipe_spawn_cannot_reverse_inherit_strict_handles() -> io::Result<()> {
        use std::collections::HashMap;
        use std::io::Read;
        use std::io::Write;
        use std::sync::Arc;
        use std::sync::Mutex as StdMutex;
        use std::sync::mpsc;
        use winapi::um::winbase::HANDLE_FLAG_INHERIT;

        if std::env::var_os(STRICT_REVERSE_TEST_HELPER).is_none() {
            let mut helper = std::process::Command::new(std::env::current_exe()?);
            helper
                .arg("--exact")
                .arg(
                    "managed_process::tests::public_pipe_spawn_cannot_reverse_inherit_strict_handles",
                )
                .arg("--nocapture")
                .env(STRICT_REVERSE_TEST_HELPER, "1");
            let mut helper = crate::with_windows_child_creation(|_| helper.spawn())?;
            let status = helper.wait()?;
            assert!(
                status.success(),
                "isolated reverse-leak helper failed: {status}"
            );
            return Ok(());
        }

        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let managed_root_baseline = MANAGED_ROOT_COUNT.load(Ordering::Acquire);
        // Keep the strict pipe handles above the range used by the child test
        // harness so an unrelated child-local handle cannot reuse the same
        // numeric value and masquerade as inherited.
        let _handle_value_reservations = reserve_noninheritable_event_handles(4096)?;
        let strict_marker = unique_strict_helper_marker("reverse-strict");
        let pipe_marker = unique_strict_helper_marker("reverse-pipe");
        let _ = std::fs::remove_file(&strict_marker);
        let _ = std::fs::remove_file(&pipe_marker);

        let (strict_executable, strict_args, strict_env) =
            strict_helper_command("stdio", &strict_marker)?;
        let strict_cwd = std::env::current_dir()?;
        let managed = ManagedRootProcess::reserve_strict_with_reclaim().await?;

        let (exposed_tx, exposed_rx) = mpsc::sync_channel(0);
        let (restored_tx, restored_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let release_rx = Arc::new(StdMutex::new(release_rx));
        let _strict_observer =
            crate::windows_child_creation::install_strict_handle_test_observer(Arc::new({
                let release_rx = Arc::clone(&release_rx);
                move |event| match event {
                    crate::windows_child_creation::WindowsStrictHandleEvent::Inheritable(
                        handles,
                    ) => {
                        exposed_tx
                            .send(handles)
                            .expect("reverse test receives exposed strict handles");
                        release_rx
                            .lock()
                            .expect("reverse release receiver lock")
                            .recv()
                            .expect("reverse test releases strict CreateProcessW");
                    }
                    crate::windows_child_creation::WindowsStrictHandleEvent::Restored {
                        handles,
                        flags,
                    } => restored_tx
                        .send((handles, flags))
                        .expect("reverse test receives restored strict handles"),
                }
            }));

        let strict_spawn = std::thread::spawn(move || {
            managed.spawn_windows_suspended(
                strict_executable.as_os_str(),
                &strict_args,
                &strict_cwd,
                &strict_env,
            )
        });
        let exposed_handles = exposed_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|error| io::Error::other(format!("strict exposure hook failed: {error}")))?;

        let (event_tx, event_rx) = mpsc::sync_channel(16);
        let _coordinator_observer =
            crate::windows_child_creation::install_windows_child_creation_test_observer(Arc::new(
                move |event| {
                    event_tx
                        .send(event)
                        .expect("reverse test receives coordinator event");
                },
            ));

        let (pipe_executable, pipe_args, pipe_env) =
            strict_helper_command("reverse-handle-probe", &pipe_marker)?;
        let pipe_program = pipe_executable.to_string_lossy().into_owned();
        let pipe_args = pipe_args
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let mut pipe_env = pipe_env
            .into_iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect::<HashMap<_, _>>();
        pipe_env.insert(
            STRICT_HELPER_REVERSE_HANDLES.to_string(),
            exposed_handles.map(|handle| handle.to_string()).join(","),
        );
        let pipe_cwd = std::env::current_dir()?;
        let pipe_spawn = tokio::spawn(async move {
            crate::spawn_pipe_process_no_stdin(
                &pipe_program,
                &pipe_args,
                &pipe_cwd,
                &pipe_env,
                &None,
            )
            .await
        });
        tokio::task::yield_now().await;

        assert_eq!(
            event_rx
                .recv_timeout(Duration::from_secs(10))
                .map_err(|error| {
                    io::Error::other(format!("public pipe did not attempt coordinator: {error}"))
                })?,
            crate::windows_child_creation::WindowsChildCreationEvent::Attempting
        );
        assert!(
            event_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "public pipe acquired while strict handles were exposed"
        );
        assert!(
            !pipe_spawn.is_finished(),
            "public pipe returned a child while strict handles were exposed"
        );

        release_tx
            .send(())
            .map_err(|error| io::Error::other(format!("release strict spawn: {error}")))?;
        let (restored_handles, restored_flags) = restored_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|error| {
                io::Error::other(format!("strict restoration hook failed: {error}"))
            })?;
        assert_eq!(restored_handles, exposed_handles);
        assert!(
            restored_flags
                .iter()
                .all(|flags| flags & HANDLE_FLAG_INHERIT == 0),
            "strict child handle flags were not restored exactly: {restored_flags:?}"
        );
        let mut saw_release = false;
        loop {
            match event_rx
                .recv_timeout(Duration::from_secs(10))
                .map_err(|error| {
                    io::Error::other(format!(
                        "coordinator did not advance after release: {error}"
                    ))
                })? {
                crate::windows_child_creation::WindowsChildCreationEvent::Released => {
                    saw_release = true
                }
                crate::windows_child_creation::WindowsChildCreationEvent::Acquired => {
                    assert!(saw_release, "public pipe acquired before strict release");
                    break;
                }
                crate::windows_child_creation::WindowsChildCreationEvent::Attempting => {
                    panic!("unexpected second public pipe coordinator attempt")
                }
            }
        }

        let (managed, mut strict_child) = strict_spawn
            .join()
            .map_err(|_| io::Error::other("strict spawn thread panicked"))??;
        assert_eq!(managed.active_process_count()?, 1);
        let pipe_child = pipe_spawn
            .await
            .map_err(|error| io::Error::other(format!("public pipe task failed: {error}")))?
            .map_err(io::Error::other)?;
        let pipe_exit = tokio::time::timeout(Duration::from_secs(10), pipe_child.exit_rx)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "public pipe did not exit"))?
            .map_err(|error| {
                io::Error::other(format!("public pipe exit channel closed: {error}"))
            })?;
        assert_eq!(pipe_exit, 0);
        wait_for_marker(&pipe_marker)?;
        assert_eq!(std::fs::read_to_string(&pipe_marker)?, "0,0,0");
        pipe_child.session.finish();

        let mut strict_stdin = strict_child.take_stdin().expect("strict child stdin");
        let mut strict_stdout = strict_child.take_stdout().expect("strict child stdout");
        strict_child.resume_primary_thread()?;
        wait_for_marker(&strict_marker)?;
        strict_stdin.write_all(b"reverse-runtime-path\n")?;
        drop(strict_stdin);
        let status = strict_child
            .wait_blocking_until(std::time::Instant::now() + Duration::from_secs(10))?;
        assert!(status.success());
        let mut output = String::new();
        strict_stdout.read_to_string(&mut output)?;
        assert!(
            output.contains("strict-echo:reverse-runtime-path"),
            "{output:?}"
        );

        drop(strict_child);
        assert_eq!(managed.active_process_count()?, 0);
        drop(managed);
        assert_eq!(
            MANAGED_ROOT_COUNT.load(Ordering::Acquire),
            managed_root_baseline,
            "reverse-leak test must release its managed-root admission"
        );
        std::fs::remove_file(strict_marker)?;
        std::fs::remove_file(pipe_marker)?;
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn strict_spawn_post_creation_errors_leave_no_process() -> io::Result<()> {
        let is_reset_failure_helper = std::env::var_os(STRICT_RESET_FAILURE_TEST_HELPER).is_some();
        if !is_reset_failure_helper {
            let mut command = std::process::Command::new(std::env::current_exe()?);
            command
                .arg("--exact")
                .arg("managed_process::tests::strict_spawn_post_creation_errors_leave_no_process")
                .arg("--nocapture")
                .env(STRICT_RESET_FAILURE_TEST_HELPER, "1");
            let mut child = crate::with_windows_child_creation(|_| command.spawn())?;
            let status = child.wait()?;
            assert!(
                status.success(),
                "isolated reset-failure helper failed: {status}"
            );
            return Ok(());
        }

        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let managed_root_baseline = MANAGED_ROOT_COUNT.load(Ordering::Acquire);
        for fault in [
            WindowsSuspendedSpawnFault::FileDescriptorConversion,
            WindowsSuspendedSpawnFault::InheritanceReset,
        ] {
            let marker = unique_strict_helper_marker(&format!("fault-{fault:?}"));
            let _ = std::fs::remove_file(&marker);
            let (executable, args, env) = strict_helper_command("stdio", &marker)?;
            let managed = ManagedRootProcess::reserve_strict_with_reclaim().await?;
            let control = managed.windows_job_control();
            let error = match managed.spawn_windows_suspended_with_fault(
                executable.as_os_str(),
                &args,
                &std::env::current_dir()?,
                &env,
                fault,
            ) {
                Ok(_) => panic!("injected {fault:?} error unexpectedly succeeded"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("injected"), "{error}");
            assert_eq!(control.active_process_count()?, 0);
            assert!(
                !marker.exists(),
                "failed suspended child must never execute"
            );
            assert_eq!(
                MANAGED_ROOT_COUNT.load(Ordering::Acquire),
                managed_root_baseline,
                "post-creation failure must release its managed-root admission"
            );
        }
        for _ in 0..2 {
            let error = crate::with_windows_child_creation(|_| Ok(()))
                .expect_err("injected reset failure must permanently compromise the scope");
            assert!(error.to_string().contains("compromised"), "{error}");
        }
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn strict_job_termination_reaps_root_and_descendant() -> io::Result<()> {
        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let marker = unique_strict_helper_marker("descendant");
        let _ = std::fs::remove_file(&marker);
        let (executable, args, env) = strict_helper_command("descendant-root", &marker)?;
        let managed = ManagedRootProcess::reserve_strict_with_reclaim().await?;
        let control = managed.windows_job_control();
        let (managed, mut child) = managed.spawn_windows_suspended(
            executable.as_os_str(),
            &args,
            &std::env::current_dir()?,
            &env,
        )?;
        child.resume_primary_thread()?;
        wait_for_marker(&marker)?;
        assert!(control.active_process_count()? >= 2);

        control.terminate()?;
        let status =
            child.wait_blocking_until(std::time::Instant::now() + Duration::from_secs(10))?;
        assert!(!status.success(), "terminated root unexpectedly succeeded");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while control.active_process_count()? != 0 {
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "strict Job still contained a process after termination",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        drop(child);
        drop(managed);
        std::fs::remove_file(marker)?;
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_child_owner_retains_admission_until_descendants_are_reaped() -> io::Result<()>
    {
        if run_managed_root_test_isolated(
            "managed_process::tests::cancelled_child_owner_retains_admission_until_descendants_are_reaped",
        )? {
            return Ok(());
        }
        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let baseline = MANAGED_ROOT_COUNT.load(Ordering::Acquire);
        let marker = unique_strict_helper_marker("cancelled-descendant");
        let _ = std::fs::remove_file(&marker);
        let (executable, args, env) = strict_helper_command("descendant-root", &marker)?;
        let managed = ManagedRootProcess::reserve_strict_with_reclaim().await?;
        let control = managed.windows_job_control();
        let (managed, mut child) = managed.spawn_windows_suspended(
            executable.as_os_str(),
            &args,
            &std::env::current_dir()?,
            &env,
        )?;
        child.resume_primary_thread()?;
        wait_for_marker(&marker)?;
        assert!(control.active_process_count()? >= 2);

        drop(managed);
        assert_eq!(
            MANAGED_ROOT_COUNT.load(Ordering::Acquire),
            baseline + 1,
            "the managed child must retain its admission lease"
        );

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let owner = tokio::spawn(async move {
            let child = child;
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
            drop(child);
        });
        started_rx.await.expect("cancelled owner task started");
        owner.abort();
        assert!(
            owner
                .await
                .expect_err("owner task must be cancelled")
                .is_cancelled(),
            "owner task ended for an unexpected reason"
        );

        assert_eq!(control.active_process_count()?, 0);
        assert_eq!(MANAGED_ROOT_COUNT.load(Ordering::Acquire), baseline);
        std::fs::remove_file(marker)?;
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn strict_spawn_preserves_real_unicode_empty_and_case_insensitive_environment()
    -> io::Result<()> {
        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let marker = unique_strict_helper_marker("environment");
        let _ = std::fs::remove_file(&marker);
        let (executable, args, mut env) = strict_helper_command("environment-probe", &marker)?;
        env.insert(
            OsString::from(STRICT_HELPER_UNICODE_ENV),
            OsString::from("välue 世界 🦀"),
        );
        env.insert(OsString::from(STRICT_HELPER_EMPTY_ENV), OsString::new());
        let managed = ManagedRootProcess::reserve_strict_with_reclaim().await?;
        let (managed, mut child) = managed.spawn_windows_suspended(
            executable.as_os_str(),
            &args,
            &std::env::current_dir()?,
            &env,
        )?;
        child.resume_primary_thread()?;
        let status =
            child.wait_blocking_until(std::time::Instant::now() + Duration::from_secs(10))?;
        assert!(status.success());
        wait_for_marker(&marker)?;
        assert_eq!(std::fs::read_to_string(&marker)?, "välue 世界 🦀\n0");

        drop(child);
        drop(managed);
        std::fs::remove_file(marker)?;

        let marker = unique_strict_helper_marker("duplicate-environment");
        let (executable, args, mut env) = strict_helper_command("environment-probe", &marker)?;
        env.insert(OsString::from("KD4_DUPLICATE_CASE"), OsString::from("a"));
        env.insert(OsString::from("kd4_duplicate_case"), OsString::from("b"));
        let managed = ManagedRootProcess::reserve_strict_with_reclaim().await?;
        let error = match managed.spawn_windows_suspended(
            executable.as_os_str(),
            &args,
            &std::env::current_dir()?,
            &env,
        ) {
            Ok(_) => panic!("case-insensitive duplicate environment names unexpectedly launched"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error
                .to_string()
                .contains("duplicate Windows environment name")
        );
        assert!(!marker.exists());
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn strict_spawn_executes_complex_real_windows_argv() -> io::Result<()> {
        use std::os::windows::ffi::OsStrExt;

        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let marker = unique_strict_helper_marker("argv");
        let _ = std::fs::remove_file(&marker);
        let expected = vec![
            OsString::from("plain"),
            OsString::from("two words"),
            OsString::from("quote\"inside"),
            OsString::from("C:\\path with spaces\\"),
            OsString::from("雪☃🦀"),
            OsString::from("tab\tinside"),
            OsString::new(),
        ];
        let (executable, mut args, env) = strict_helper_command("argv-probe", &marker)?;
        args.push(OsString::from("--"));
        args.extend(expected.iter().cloned());
        let managed = ManagedRootProcess::reserve_strict_with_reclaim().await?;
        let (managed, mut child) = managed.spawn_windows_suspended(
            executable.as_os_str(),
            &args,
            &std::env::current_dir()?,
            &env,
        )?;
        assert!(
            !marker.exists(),
            "suspended argument probe must not execute"
        );
        child.resume_primary_thread()?;
        let status =
            child.wait_blocking_until(std::time::Instant::now() + Duration::from_secs(10))?;
        assert!(status.success());
        wait_for_marker(&marker)?;
        let actual = std::fs::read_to_string(&marker)?;
        let expected = expected
            .iter()
            .map(|value| {
                let units = value.encode_wide().collect::<Vec<_>>();
                let mut encoded = units.len().to_string();
                for unit in units {
                    encoded.push(',');
                    encoded.push_str(&unit.to_string());
                }
                encoded
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(actual, expected);

        drop(child);
        drop(managed);
        std::fs::remove_file(marker)?;
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn strict_spawn_executes_cmd_c_payload() -> io::Result<()> {
        use std::io::Read;

        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let executable = std::env::var_os("ComSpec")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));
        let args = [
            OsString::from("/d"),
            OsString::from("/s"),
            OsString::from("/c"),
            OsString::from("echo strict cmd payload"),
        ];
        let env = std::env::vars_os().collect::<BTreeMap<_, _>>();
        let managed = ManagedRootProcess::reserve_strict_with_reclaim().await?;
        let (managed, mut child) = managed.spawn_windows_suspended(
            executable.as_os_str(),
            &args,
            &std::env::current_dir()?,
            &env,
        )?;
        let mut stdout = child.take_stdout().expect("strict cmd stdout");
        child.close_stdin();
        child.resume_primary_thread()?;
        let status =
            child.wait_blocking_until(std::time::Instant::now() + Duration::from_secs(10))?;
        assert!(status.success());
        let mut output = String::new();
        stdout.read_to_string(&mut output)?;
        assert_eq!(output.trim(), "strict cmd payload");
        drop(child);
        drop(managed);
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn strict_job_denies_explicit_descendant_breakaway() -> io::Result<()> {
        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let marker = unique_strict_helper_marker("breakaway");
        let _ = std::fs::remove_file(&marker);
        let (executable, args, env) = strict_helper_command("breakaway-probe", &marker)?;
        let managed = ManagedRootProcess::reserve_strict_with_reclaim().await?;
        let (managed, mut child) = managed.spawn_windows_suspended(
            executable.as_os_str(),
            &args,
            &std::env::current_dir()?,
            &env,
        )?;
        child.resume_primary_thread()?;
        let status =
            child.wait_blocking_until(std::time::Instant::now() + Duration::from_secs(10))?;
        assert!(status.success());
        wait_for_marker(&marker)?;
        assert_eq!(std::fs::read_to_string(&marker)?, "error:5");

        drop(child);
        drop(managed);
        std::fs::remove_file(marker)?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_root_admission_reclaims_live_registrations_in_order() {
        #[cfg(windows)]
        if run_managed_root_test_isolated(
            "managed_process::tests::managed_root_admission_reclaims_live_registrations_in_order",
        )
        .expect("launch isolated managed-root reclaim-order test")
        {
            return;
        }
        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let roots = Arc::new(Mutex::new(
            (0..MANAGED_ROOT_LIMIT)
                .map(|_| ManagedRootProcess::reserve().expect("reserve root"))
                .collect::<Vec<_>>(),
        ));
        assert!(ManagedRootProcess::reserve().is_err());

        let phases = Arc::new(Mutex::new(Vec::new()));
        let older_retire_phases = Arc::clone(&phases);
        let older_evict_phases = Arc::clone(&phases);
        let older_evict_roots = Arc::clone(&roots);
        let older_guard = install_managed_root_admission_reclaimer(
            Arc::new(move || {
                let phases = Arc::clone(&older_retire_phases);
                Box::pin(async move {
                    phases.lock().expect("phases lock").push("older-retire");
                })
            }),
            Arc::new(move || {
                let phases = Arc::clone(&older_evict_phases);
                let roots = Arc::clone(&older_evict_roots);
                Box::pin(async move {
                    phases.lock().expect("phases lock").push("older-evict");
                    roots.lock().expect("roots lock").pop();
                })
            }),
        );
        let newer_retire_phases = Arc::clone(&phases);
        let newer_evict_phases = Arc::clone(&phases);
        let newer_guard = install_managed_root_admission_reclaimer(
            Arc::new(move || {
                let phases = Arc::clone(&newer_retire_phases);
                Box::pin(async move {
                    phases.lock().expect("phases lock").push("newer-retire");
                })
            }),
            Arc::new(move || {
                let phases = Arc::clone(&newer_evict_phases);
                Box::pin(async move {
                    phases.lock().expect("phases lock").push("newer-evict");
                })
            }),
        );

        let admitted = ManagedRootProcess::reserve_with_reclaim()
            .await
            .expect("reserve after one eviction");
        assert_eq!(
            phases.lock().expect("phases lock").as_slice(),
            ["newer-retire", "older-retire", "newer-evict", "older-evict"]
        );
        drop(admitted);

        roots
            .lock()
            .expect("roots lock")
            .push(ManagedRootProcess::reserve().expect("refill root"));
        drop(newer_guard);
        phases.lock().expect("phases lock").clear();
        let admitted = ManagedRootProcess::reserve_with_reclaim()
            .await
            .expect("older registration remains active");
        assert_eq!(
            phases.lock().expect("phases lock").as_slice(),
            ["older-retire", "older-evict"]
        );
        drop(admitted);
        drop(older_guard);
        roots.lock().expect("roots lock").clear();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_root_admission_times_out_a_stalled_reclaimer() {
        #[cfg(windows)]
        if run_managed_root_test_isolated(
            "managed_process::tests::managed_root_admission_times_out_a_stalled_reclaimer",
        )
        .expect("launch isolated managed-root timeout test")
        {
            return;
        }
        let _test_guard = managed_root_test_lock()
            .acquire()
            .await
            .expect("test semaphore remains open");
        let roots = Arc::new(Mutex::new(
            (0..MANAGED_ROOT_LIMIT)
                .map(|_| ManagedRootProcess::reserve().expect("reserve root"))
                .collect::<Vec<_>>(),
        ));
        let stalled_guard = install_managed_root_admission_reclaimer(
            Arc::new(|| -> ManagedRootReclaimFuture { Box::pin(std::future::pending()) }),
            Arc::new(|| -> ManagedRootReclaimFuture { Box::pin(async {}) }),
        );

        let error = match ManagedRootProcess::reserve_with_reclaim_policy(
            Duration::from_millis(25),
            /*allow_breakaway*/ true,
        )
        .await
        {
            Ok(_) => panic!("stalled reclaimer unexpectedly admitted a root"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("reclamation timed out"));

        // Timing out must drop the serialized-admission permit so a later
        // healthy reclaimer can make progress.
        drop(stalled_guard);
        let evict_roots = Arc::clone(&roots);
        let healthy_guard = install_managed_root_admission_reclaimer(
            Arc::new(|| -> ManagedRootReclaimFuture { Box::pin(async {}) }),
            Arc::new(move || {
                let roots = Arc::clone(&evict_roots);
                Box::pin(async move {
                    roots.lock().expect("roots lock").pop();
                })
            }),
        );
        let admitted = ManagedRootProcess::reserve_with_reclaim_policy(
            Duration::from_millis(250),
            /*allow_breakaway*/ true,
        )
        .await
        .expect("healthy reclaimer should recover after the timeout");

        drop(admitted);
        drop(healthy_guard);
        roots.lock().expect("roots lock").clear();
    }
}
