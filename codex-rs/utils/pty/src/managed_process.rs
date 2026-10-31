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
    id: u64,
    #[cfg(windows)]
    job: Arc<crate::win::JobObject>,
}

#[derive(Debug)]
struct AdmissionCapacityExhausted;

impl std::fmt::Display for AdmissionCapacityExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "managed root process limit reached ({MANAGED_ROOT_LIMIT})"
        )
    }
}

impl std::error::Error for AdmissionCapacityExhausted {}

fn reclaimable_reservation(
    result: io::Result<ManagedRootProcess>,
) -> io::Result<Option<ManagedRootProcess>> {
    match result {
        Ok(root) => Ok(Some(root)),
        Err(error)
            if error.get_ref().is_some_and(
                <dyn std::error::Error + Send + Sync>::is::<AdmissionCapacityExhausted>,
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}


impl ManagedRootProcess {
    /// Reserve one of the process-wide managed-root slots before spawning.
    pub fn reserve() -> io::Result<Self> {
        let mut current = MANAGED_ROOT_COUNT.load(Ordering::Acquire);
        loop {
            if current >= MANAGED_ROOT_LIMIT {
                return Err(io::Error::other(AdmissionCapacityExhausted));
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
        let job = match crate::win::JobObject::create().and_then(|job| {
            // Every managed launch promises cancellation of the contained
            // workload, including pipes and the direct core execution path.
            job.require_descendant_containment()?;
            Ok(job)
        }) {
            Ok(job) => job,
            Err(error) => {
                MANAGED_ROOT_COUNT.fetch_sub(1, Ordering::AcqRel);
                return Err(error);
            }
        };

        Ok(Self {
            id: NEXT_MANAGED_ROOT_ID.fetch_add(1, Ordering::Relaxed),
            #[cfg(windows)]
            job: Arc::new(job),
        })
    }

    /// Reserve a root, attempting one deadline-bounded serialized cross-layer
    /// reclaim pass before rejecting a launch at the hard limit.
    pub async fn reserve_with_reclaim() -> io::Result<Self> {
        Self::reserve_with_reclaim_timeout(MANAGED_ROOT_RECLAIM_TIMEOUT).await
    }

    async fn reserve_on_worker() -> io::Result<Self> {
        #[cfg(windows)]
        {
            run_windows_process_operation(WINDOWS_PROCESS_OPERATION_TIMEOUT, Self::reserve).await
        }
        #[cfg(not(windows))]
        {
            Self::reserve()
        }
    }

    async fn reserve_with_reclaim_timeout(reclaim_timeout: Duration) -> io::Result<Self> {
        if let Some(root) = reclaimable_reservation(Self::reserve_on_worker().await)? {
            return Ok(root);
        }

        let reclaim = async {
            let _reclaim = ADMISSION_RECLAIM_PERMIT
                .get_or_init(|| tokio::sync::Semaphore::new(1))
                .acquire()
                .await
                .map_err(|error| {
                    io::Error::other(format!("admission reclaimer closed: {error}"))
                })?;
            if let Some(root) = reclaimable_reservation(Self::reserve_on_worker().await)? {
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
            if let Some(root) = reclaimable_reservation(Self::reserve_on_worker().await)? {
                return Ok(root);
            }
            for reclaimer in reclaimers {
                (reclaimer.evict_one_eligible_task)().await;
                if let Some(root) = reclaimable_reservation(Self::reserve_on_worker().await)? {
                    return Ok(root);
                }
            }

            Self::reserve_on_worker().await
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
        self.id
    }


    /// Open a normally running Windows root by PID and attach it to the Job.
    #[cfg(windows)]
    pub fn attach(&self, pid: u32) -> io::Result<()> {
        self.attach_with_access(pid, 0).map(drop)
    }

    #[cfg(windows)]
    fn attach_with_access(
        &self,
        pid: u32,
        extra_access: u32,
    ) -> io::Result<std::os::windows::io::OwnedHandle> {
        use std::os::windows::io::FromRawHandle;
        use std::os::windows::io::OwnedHandle;
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::winnt::PROCESS_SET_QUOTA;
        use winapi::um::winnt::PROCESS_TERMINATE;

        // SAFETY: OpenProcess takes only scalar arguments; a successful non-null result is a
        // new owned process handle.
        let access = PROCESS_SET_QUOTA | PROCESS_TERMINATE;
        let mut raw = unsafe { OpenProcess(access | extra_access, 0, pid) };
        if raw.is_null() && extra_access != 0 {
            // Extra resume rights are optional: retain the original thread-scan path
            // on systems where only the Job assignment rights are available.
            // SAFETY: As above, this opens a new owned handle using scalar arguments.
            raw = unsafe { OpenProcess(access, 0, pid) };
        }
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: The null failure result was rejected, and this is the sole owner of the
        // handle returned by OpenProcess.
        let process = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
        self.job.assign_process(raw.cast())?;
        Ok(process)
    }

    /// Attach a `CREATE_SUSPENDED` child to the Job and then resume all of its
    /// threads. Prefer resuming through the process handle; fall back to thread
    /// enumeration when the optional native API is unavailable or fails.
    #[cfg(windows)]
    pub fn attach_and_resume(&self, pid: u32) -> io::Result<()> {
        use std::os::windows::io::AsHandle;
        use winapi::um::winnt::PROCESS_SUSPEND_RESUME;

        let process = self.attach_with_access(pid, PROCESS_SUSPEND_RESUME)?;
        resume_process(process.as_handle(), pid, nt_resume_process())
    }

    /// Prevent descendant launchers from escaping this root's Windows job.
    /// Configure this before attaching and resuming the root.
    #[cfg(windows)]
    pub fn require_descendant_containment(&self) -> io::Result<()> {
        self.job.require_descendant_containment()
    }

    #[cfg(windows)]
    pub fn terminate(&self) -> io::Result<()> {
        self.job.terminate()
    }

    #[cfg(windows)]
    pub(crate) fn tree_is_empty(&self) -> io::Result<bool> {
        self.job.active_process_count().map(|count| count == 0)
    }

    #[cfg(windows)]
    pub(crate) fn replay_job(&self) -> Arc<crate::JobObject> {
        Arc::clone(&self.job)
    }

    /// Terminate and reap a child whose containment setup failed.
    ///
    /// The caller waits at most five seconds. The cleanup task continues owning
    /// the child and admission slot until the native wait completes, including
    /// when the caller is cancelled or both termination operations fail.
    #[cfg(windows)]
    pub fn cleanup_after_failed_attach(
        self,
        child: tokio::process::Child,
    ) -> impl Future<Output = io::Result<()>> {
        self.cleanup_with_process_operations(
            child,
            Self::terminate,
            tokio::process::Child::start_kill,
            |child| Box::pin(child.wait()),
            true,
        )
    }

    /// Terminate a contained process tree and retain its admission until reaped.
    /// The caller has the same five-second limit as failed-attachment cleanup.
    #[cfg(windows)]
    pub fn terminate_and_reap(
        self,
        child: tokio::process::Child,
    ) -> impl Future<Output = io::Result<()>> {
        self.cleanup_with_process_operations(
            child,
            Self::terminate,
            tokio::process::Child::start_kill,
            |child| Box::pin(child.wait()),
            false,
        )
    }

    #[cfg(windows)]
    fn cleanup_with_process_operations<J, K, W>(
        self,
        mut child: tokio::process::Child,
        terminate_job: J,
        terminate_child: K,
        wait_for_child: W,
        always_terminate_root: bool,
    ) -> impl Future<Output = io::Result<()>>
    where
        J: FnOnce(&Self) -> io::Result<()> + Send + 'static,
        K: FnOnce(&mut tokio::process::Child) -> io::Result<()> + Send + 'static,
        W: for<'a> FnOnce(
                &'a mut tokio::process::Child,
            ) -> Pin<
                Box<dyn Future<Output = io::Result<std::process::ExitStatus>> + Send + 'a>,
            > + Send
            + 'static,
    {
        // Spawn before returning the future: dropping an unpolled caller must
        // not discard the cleanup owner or release admission before reaping.
        let runtime = tokio::runtime::Handle::current();
        let cleanup = tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                let job_error = terminate_job(&self).err();
                if let Some(error) = &job_error {
                    log::warn!("failed to terminate managed process job: {error}");
                }
                // Failed attachment always needs a root attempt, even if an empty
                // job terminates successfully. A contained tree needs that fallback
                // only when job termination fails.
                let kill_error = if always_terminate_root || job_error.is_some() {
                    terminate_child(&mut child).err()
                } else {
                    None
                };
                if let Some(error) = &kill_error {
                    log::warn!("failed to terminate managed root process: {error}");
                }
                if let Err(error) = wait_for_child(&mut child).await {
                    log::warn!(
                        "failed to wait for managed root process; retaining admission and polling for exit: {error}"
                    );
                    // Windows can fail to register its native wait while the child
                    // is alive. Neither that error nor a failed status query proves
                    // exit, so retain ownership until a real status is observed.
                    loop {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        if let Ok(Some(_)) = child.try_wait() {
                            break;
                        }
                    }
                }
                // Reaping the root cannot settle a failed tree termination.
                // Reuse the runtime-independent reconciler: an indefinitely
                // failing job must not pin Tokio's blocking pool at shutdown.
                if let Some(error) = job_error {
                    crate::process::retain_terminator(Box::new(self));
                    return Err(error);
                }
                drop(self);
                Ok(())
            })
        });
        async move {
            tokio::time::timeout(Duration::from_secs(5), cleanup)
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "managed process cleanup did not finish within five seconds; its owner is still waiting for the root process",
                    )
                })?
                .map_err(|error| io::Error::other(format!("managed process cleanup task failed: {error}")))?
        }
    }

    #[cfg(windows)]
    pub fn preserve_descendants(&self) -> io::Result<()> {
        self.job.preserve_descendants()
    }
}

#[cfg(windows)]
impl crate::process::ChildTerminator for ManagedRootProcess {
    fn signal(&mut self, signal: crate::process::ProcessSignal) -> io::Result<()> {
        Err(crate::process::unsupported_signal(signal))
    }

    fn kill(&mut self) -> io::Result<()> {
        self.terminate()
    }

    fn tree_is_empty(&self) -> io::Result<bool> {
        ManagedRootProcess::tree_is_empty(self)
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
    let observation = crate::process_observation::ProcessObservationContext::current();
    tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || {
            crate::process_observation::ProcessObservationContext::scope_sync(
                observation,
                operation,
            )
        }),
    )
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
type NtResumeProcess =
    unsafe extern "system" fn(winapi::um::winnt::HANDLE) -> winapi::shared::ntdef::NTSTATUS;

#[cfg(windows)]
fn nt_resume_process() -> Option<NtResumeProcess> {
    use winapi::shared::minwindef::FARPROC;
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::libloaderapi::GetProcAddress;

    static RESUME: OnceLock<Option<NtResumeProcess>> = OnceLock::new();
    *RESUME.get_or_init(|| {
        let module_name = "ntdll.dll\0".encode_utf16().collect::<Vec<_>>();
        // SAFETY: module_name is NUL-terminated. ntdll is a process-lifetime system
        // module; this borrowed module handle must not be passed to FreeLibrary.
        let module = unsafe { GetModuleHandleW(module_name.as_ptr()) };
        if module.is_null() {
            return None;
        }
        // SAFETY: module is live and the export name is NUL-terminated.
        let address = unsafe { GetProcAddress(module, c"NtResumeProcess".as_ptr()) };
        if address.is_null() {
            return None;
        }
        // SAFETY: NtResumeProcess takes a process HANDLE and returns NTSTATUS using
        // the Windows system ABI. ntdll remains loaded for the cached pointer's lifetime.
        Some(unsafe { std::mem::transmute::<FARPROC, NtResumeProcess>(address) })
    })
}

#[cfg(windows)]
fn resume_process(
    process: std::os::windows::io::BorrowedHandle<'_>,
    pid: u32,
    native_resume: Option<NtResumeProcess>,
) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    if let Some(native_resume) = native_resume {
        // SAFETY: The borrowed process handle remains live for this call. Windows
        // checks its access rights; NTSTATUS is successful when nonnegative.
        if unsafe { native_resume(process.as_raw_handle().cast()) } >= 0 {
            return Ok(());
        }
    }
    // NtResumeProcess is undocumented: preserve the supported thread APIs as a
    // compatibility path rather than making successful process creation depend on it.
    resume_process_threads(pid)
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

    // SAFETY: The snapshot flags and PID are scalar values; failure is checked before the
    // returned handle is used.
    let raw_snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw_snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateToolhelp32Snapshot succeeded and transfers one handle, which this
    // OwnedHandle closes exactly once.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw_snapshot.cast()) };
    // SAFETY: THREADENTRY32 contains only integer fields; zero is a valid value for every field
    // before dwSize is set.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    // SAFETY: snapshot owns a live snapshot handle; entry is writable and its dwSize describes
    // the complete THREADENTRY32.
    let mut has_entry = unsafe { Thread32First(snapshot.as_raw_handle().cast(), &mut entry) } != 0;
    let mut resumed = 0usize;

    while has_entry {
        if entry.th32OwnerProcessID == pid {
            let raw_thread =
                // SAFETY: The requested access and thread ID are scalar arguments; the returned
                // handle is checked for failure.
                unsafe { OpenThread(THREAD_SUSPEND_RESUME, FALSE, entry.th32ThreadID) };
            if raw_thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: OpenThread returned a non-null owned handle that has no other owning
            // wrapper.
            let thread = unsafe { OwnedHandle::from_raw_handle(raw_thread.cast()) };
            // SAFETY: thread owns the live handle opened with THREAD_SUSPEND_RESUME access for
            // this call.
            if unsafe { ResumeThread(thread.as_raw_handle().cast()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            resumed += 1;
        }
        // SAFETY: snapshot and the writable, size-initialized entry remain alive throughout
        // enumeration.
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

impl Drop for ManagedRootProcess {
    fn drop(&mut self) {
        let remaining = MANAGED_ROOT_COUNT.fetch_sub(1, Ordering::AcqRel) - 1;
        log::debug!(
            "released managed root process lifecycle_id={} remaining={remaining}",
            self.id
        );
    }
}

