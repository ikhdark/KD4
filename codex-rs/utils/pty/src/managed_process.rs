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
    job: crate::win::JobObject,
}

impl ManagedRootProcess {
    /// Reserve one of the process-wide managed-root slots before spawning.
    pub fn reserve() -> io::Result<Self> {
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
        let job = match crate::win::JobObject::create() {
            Ok(job) => job,
            Err(error) => {
                MANAGED_ROOT_COUNT.fetch_sub(1, Ordering::AcqRel);
                return Err(error);
            }
        };

        Ok(Self {
            id: NEXT_MANAGED_ROOT_ID.fetch_add(1, Ordering::Relaxed),
            #[cfg(windows)]
            job,
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
        match Self::reserve_on_worker().await {
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
            if let Ok(root) = Self::reserve_on_worker().await {
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
            if let Ok(root) = Self::reserve_on_worker().await {
                return Ok(root);
            }
            for reclaimer in reclaimers {
                (reclaimer.evict_one_eligible_task)().await;
                if let Ok(root) = Self::reserve_on_worker().await {
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

    #[cfg(all(test, windows))]
    pub(crate) fn restrict_job_to_query_access_for_test(&mut self) -> io::Result<()> {
        self.job.restrict_to_query_access_for_test()
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
                drop(self);
                match kill_error {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
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

impl Drop for ManagedRootProcess {
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

    fn managed_root_test_lock() -> &'static tokio::sync::Semaphore {
        static TEST_LOCK: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
        TEST_LOCK.get_or_init(|| tokio::sync::Semaphore::new(1))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_root_admission_reclaims_live_registrations_in_order() {
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

        let error =
            match ManagedRootProcess::reserve_with_reclaim_timeout(Duration::from_millis(25)).await
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
        let admitted = ManagedRootProcess::reserve_with_reclaim_timeout(Duration::from_millis(250))
            .await
            .expect("healthy reclaimer should recover after the timeout");

        drop(admitted);
        drop(healthy_guard);
        roots.lock().expect("roots lock").clear();
    }

    #[cfg(windows)]
    struct RealCleanupChild {
        handle: std::os::windows::io::OwnedHandle,
    }

    #[cfg(windows)]
    impl RealCleanupChild {
        fn observe(child: &tokio::process::Child) -> Self {
            use std::os::windows::io::FromRawHandle;
            use winapi::um::processthreadsapi::OpenProcess;
            use winapi::um::winnt::PROCESS_TERMINATE;
            use winapi::um::winnt::SYNCHRONIZE;

            // SAFETY: OpenProcess receives a live child PID and returns a new
            // owned handle used only for observation and panic-safe cleanup.
            let raw =
                unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, child.id().unwrap()) };
            assert!(
                !raw.is_null(),
                "open cleanup child: {}",
                io::Error::last_os_error()
            );
            Self {
                handle: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw.cast()) },
            }
        }

        fn has_exited(&self) -> bool {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: the owned process handle remains live for this zero-wait observation.
            unsafe {
                winapi::um::synchapi::WaitForSingleObject(self.handle.as_raw_handle().cast(), 0)
                    == winapi::um::winbase::WAIT_OBJECT_0
            }
        }
    }

    #[cfg(windows)]
    impl Drop for RealCleanupChild {
        fn drop(&mut self) {
            use std::os::windows::io::AsRawHandle;
            if !self.has_exited() {
                // SAFETY: this is the owned process handle with terminate and
                // synchronize rights; it prevents leaked children after a failed assertion.
                unsafe {
                    winapi::um::processthreadsapi::TerminateProcess(
                        self.handle.as_raw_handle().cast(),
                        1,
                    );
                    winapi::um::synchapi::WaitForSingleObject(
                        self.handle.as_raw_handle().cast(),
                        5_000,
                    );
                }
            }
        }
    }

    #[cfg(windows)]
    fn spawn_cleanup_child() -> tokio::process::Child {
        let mut command = tokio::process::Command::new("cmd.exe");
        command.args(["/D", "/Q", "/C", "set /p CODEX_CLEANUP_WAIT="]);
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::null());
        command.kill_on_drop(true);
        command
            .spawn()
            .expect("spawn real input-blocked cleanup child")
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn failed_attach_cleanup_reaps_real_child_after_native_job_failure_even_if_caller_dropped()
     {
        let _test_guard = managed_root_test_lock().acquire().await.unwrap();
        let mut root = ManagedRootProcess::reserve().unwrap();
        root.job.restrict_to_query_access_for_test().unwrap();
        let mut child = spawn_cleanup_child();
        let _stdin = child.stdin.take().unwrap();
        let observed = RealCleanupChild::observe(&child);
        assert!(
            root.attach(child.id().unwrap()).is_err(),
            "restricted native job must reject assignment"
        );
        assert!(
            root.terminate().is_err(),
            "restricted native job must reject termination"
        );
        assert!(!observed.has_exited());
        let cleanup = root.cleanup_after_failed_attach(child);
        drop(cleanup);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !observed.has_exited() || MANAGED_ROOT_COUNT.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("normal cleanup must terminate and reap despite an unpolled caller");
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn failed_attach_cleanup_kills_unattached_root_even_when_empty_job_termination_succeeds()
    {
        let _test_guard = managed_root_test_lock().acquire().await.unwrap();
        let root = ManagedRootProcess::reserve().unwrap();
        let mut child = spawn_cleanup_child();
        let _stdin = child.stdin.take().unwrap();
        let observed = RealCleanupChild::observe(&child);
        assert!(!observed.has_exited());
        tokio::time::timeout(
            Duration::from_secs(8),
            root.cleanup_after_failed_attach(child),
        )
        .await
        .expect("cleanup caller must remain bounded")
        .expect("an unattached root must be terminated and reaped");
        assert!(observed.has_exited());
        assert_eq!(MANAGED_ROOT_COUNT.load(Ordering::Acquire), 0);
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn failed_attach_cleanup_deadline_retains_admission_until_real_child_exits_after_kill_errors()
     {
        use tokio::io::AsyncWriteExt;
        let _test_guard = managed_root_test_lock().acquire().await.unwrap();
        let mut root = ManagedRootProcess::reserve().unwrap();
        root.job.restrict_to_query_access_for_test().unwrap();
        let mut child = spawn_cleanup_child();
        let mut stdin = child.stdin.take().unwrap();
        let observed = RealCleanupChild::observe(&child);
        assert!(root.attach(child.id().unwrap()).is_err());
        let held_roots = (1..MANAGED_ROOT_LIMIT)
            .map(|_| ManagedRootProcess::reserve().unwrap())
            .collect::<Vec<_>>();
        assert!(ManagedRootProcess::reserve().is_err());
        let job_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let child_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let job_attempt = Arc::clone(&job_attempted);
        let child_attempt = Arc::clone(&child_attempted);
        let cleanup = root.cleanup_with_process_operations(
            child,
            move |_| {
                job_attempt.store(true, Ordering::Release);
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected external job termination failure",
                ))
            },
            move |_| {
                child_attempt.store(true, Ordering::Release);
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected external root termination failure",
                ))
            },
            |child| Box::pin(child.wait()),
            true,
        );
        let error = tokio::time::timeout(Duration::from_secs(8), cleanup)
            .await
            .expect("cleanup must return within its deadline")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(job_attempted.load(Ordering::Acquire));
        assert!(child_attempted.load(Ordering::Acquire));
        assert!(
            !observed.has_exited(),
            "failed termination must leave this real child alive until input is released"
        );
        assert!(
            ManagedRootProcess::reserve().is_err(),
            "timing out the caller must not release the live root's admission"
        );
        stdin.write_all(b"release\r\n").await.unwrap();
        drop(stdin);
        let admitted = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(root) = ManagedRootProcess::reserve() {
                    break root;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("real exit and reap must release admission");
        assert!(
            observed.has_exited(),
            "admission must not be released before native process exit"
        );
        drop(admitted);
        drop(held_roots);
    }
    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn failed_attach_cleanup_wait_error_retains_admission_until_native_exit_is_observed() {
        use tokio::io::AsyncWriteExt;
        let _test_guard = managed_root_test_lock().acquire().await.unwrap();
        let mut root = ManagedRootProcess::reserve().unwrap();
        root.job.restrict_to_query_access_for_test().unwrap();
        let mut child = spawn_cleanup_child();
        let mut stdin = child.stdin.take().unwrap();
        let observed = RealCleanupChild::observe(&child);
        assert!(root.attach(child.id().unwrap()).is_err());
        let held_roots = (1..MANAGED_ROOT_LIMIT)
            .map(|_| ManagedRootProcess::reserve().unwrap())
            .collect::<Vec<_>>();
        assert!(ManagedRootProcess::reserve().is_err());
        let job_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let child_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let job_attempt = Arc::clone(&job_attempted);
        let child_attempt = Arc::clone(&child_attempted);
        let cleanup = root.cleanup_with_process_operations(
            child,
            move |_| {
                job_attempt.store(true, Ordering::Release);
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected external job termination failure",
                ))
            },
            move |_| {
                child_attempt.store(true, Ordering::Release);
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected external root termination failure",
                ))
            },
            |_| {
                Box::pin(async {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "injected external native wait registration failure",
                    ))
                })
            },
            true,
        );
        let error = tokio::time::timeout(Duration::from_secs(8), cleanup)
            .await
            .expect("cleanup must return within its deadline")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(job_attempted.load(Ordering::Acquire));
        assert!(child_attempted.load(Ordering::Acquire));
        assert!(
            !observed.has_exited(),
            "failed termination must leave this real child alive until input is released"
        );
        assert!(
            ManagedRootProcess::reserve().is_err(),
            "timing out the caller must not release the live root's admission"
        );
        stdin.write_all(b"release\r\n").await.unwrap();
        drop(stdin);
        let admitted = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(root) = ManagedRootProcess::reserve() {
                    break root;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("real exit and reap must release admission");
        assert!(
            observed.has_exited(),
            "admission must not be released before native process exit"
        );
        drop(admitted);
        drop(held_roots);
    }

    #[cfg(windows)]
    #[test]
    fn contained_cleanup_queues_native_work_and_retains_child_when_caller_is_dropped() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let _test_guard = managed_root_test_lock().acquire().await.unwrap();
            let root = ManagedRootProcess::reserve().unwrap();
            let mut child = spawn_cleanup_child();
            let _stdin = child.stdin.take().unwrap();
            root.attach(child.id().unwrap())
                .expect("normal native containment");
            let observed = RealCleanupChild::observe(&child);
            let held_roots = (1..MANAGED_ROOT_LIMIT)
                .map(|_| ManagedRootProcess::reserve().unwrap())
                .collect::<Vec<_>>();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let occupied = tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                release_rx.recv_timeout(Duration::from_secs(5)).is_ok()
            });
            started_rx.await.unwrap();
            let cleanup = root.terminate_and_reap(child);
            drop(cleanup);
            tokio::time::sleep(Duration::from_millis(25)).await;
            assert!(
                !occupied.is_finished(),
                "runtime advances while native worker is occupied"
            );
            assert!(
                !observed.has_exited(),
                "queued cleanup must not kill inline"
            );
            assert!(
                ManagedRootProcess::reserve().is_err(),
                "unpolled caller Drop must retain live admission"
            );
            release_tx.send(()).unwrap();
            assert!(occupied.await.unwrap());
            let admitted = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(root) = ManagedRootProcess::reserve() {
                        break root;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("owned native cleanup releases admission after real exit");
            assert!(observed.has_exited());
            drop(admitted);
            drop(held_roots);
        });
    }

    #[cfg(windows)]
    #[test]
    fn async_root_admission_queues_native_creation_and_releases_canceled_reservation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let _test_guard = managed_root_test_lock().acquire().await.unwrap();
            let held_roots = (1..MANAGED_ROOT_LIMIT)
                .map(|_| ManagedRootProcess::reserve().unwrap())
                .collect::<Vec<_>>();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                release_rx.recv_timeout(Duration::from_secs(5)).is_ok()
            });
            started_rx.await.unwrap();
            let mut pending = Box::pin(ManagedRootProcess::reserve_with_reclaim());
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut pending)
                    .await
                    .is_err(),
                "native Job creation must wait for the worker while the caller timer advances"
            );
            assert!(!blocker.is_finished());
            drop(pending);
            release_tx.send(()).unwrap();
            assert!(blocker.await.unwrap());
            let root = tokio::time::timeout(
                Duration::from_secs(5),
                ManagedRootProcess::reserve_with_reclaim(),
            )
            .await
            .expect("queued native allocation completes")
            .expect("canceled allocation must release its only available admission slot");
            assert!(ManagedRootProcess::reserve().is_err());
            let mut child = spawn_cleanup_child();
            let _stdin = child.stdin.take().unwrap();
            let observed = RealCleanupChild::observe(&child);
            root.attach(child.id().unwrap())
                .expect("worker-created native Job accepts a real process");
            root.terminate_and_reap(child)
                .await
                .expect("worker-created native Job terminates its actual process");
            assert!(observed.has_exited());
            let recovered = ManagedRootProcess::reserve().expect("reaped root releases admission");
            drop(recovered);
            drop(held_roots);
        });
    }
}
