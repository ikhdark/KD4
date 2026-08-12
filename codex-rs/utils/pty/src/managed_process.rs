use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

const MANAGED_ROOT_WARNING_THRESHOLD: usize = 384;
const MANAGED_ROOT_LIMIT: usize = 512;

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

    /// Reserve a root, running one serialized cross-layer reclaim pass before
    /// rejecting a launch at the hard limit.
    pub async fn reserve_with_reclaim() -> io::Result<Self> {
        match Self::reserve() {
            Ok(root) => return Ok(root),
            Err(error) if MANAGED_ROOT_COUNT.load(Ordering::Acquire) < MANAGED_ROOT_LIMIT => {
                return Err(error);
            }
            Err(_) => {}
        }

        let _reclaim = ADMISSION_RECLAIM_PERMIT
            .get_or_init(|| tokio::sync::Semaphore::new(1))
            .acquire()
            .await
            .map_err(|error| io::Error::other(format!("admission reclaimer closed: {error}")))?;
        if let Ok(root) = Self::reserve() {
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
        if let Ok(root) = Self::reserve() {
            return Ok(root);
        }
        for reclaimer in reclaimers {
            (reclaimer.evict_one_eligible_task)().await;
            if let Ok(root) = Self::reserve() {
                return Ok(root);
            }
        }

        Self::reserve()
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Open a normally running Windows root by PID and attach it to the Job.
    ///
    /// Do not pair this with `CREATE_SUSPENDED`: Tokio does not retain the
    /// primary thread handle needed for a reliable `ResumeThread` call.
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

    #[cfg(windows)]
    pub fn terminate(&self) -> io::Result<()> {
        self.job.terminate()
    }

    #[cfg(windows)]
    pub fn preserve_descendants(&self) -> io::Result<()> {
        self.job.preserve_descendants()
    }
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

    #[tokio::test(flavor = "current_thread")]
    async fn managed_root_admission_reclaims_live_registrations_in_order() {
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
}
