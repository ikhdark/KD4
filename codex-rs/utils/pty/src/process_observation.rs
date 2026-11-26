//! Root observations at successful creation boundaries, not a launcher.
//! Counts cover only instrumented roots; opaque descendants remain unknown.
use std::cell::RefCell;
use std::future::Future;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use serde::Serialize;

const DETAIL_LIMIT: usize = 256;
static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);
static RECORDER_ID: LazyLock<String> =
    LazyLock::new(|| format!("{}-{}", std::process::id(), unix_ms()));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRole {
    RequestedRoot,
    AuxiliaryRoot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DescendantCleanupObservation {
    ContainedWindowsJobEmpty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessStart {
    /// Local image observation; absent for remote/opaque or inaccessible roots.
    pub resolved_executable: Option<String>,
    pub executable_identity: crate::environment_receipt::ExecutableIdentity,
    pub environment: Option<crate::environment_receipt::LaunchEnvironment>,
    pub instance_id: String,
    pub execution_id: Option<String>,
    pub producer: String,
    pub role: ProcessRole,
    pub os_pid: Option<u32>,
    /// An acknowledgment identifies a remote backend root, never a local OS PID.
    pub backend_identity: Option<String>,
    pub provenance: String,
    pub observed_at_unix_ms: u64,
    pub cleanup_observation: Option<String>,
    /// Separate native containment evidence, not an inference from root exit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descendant_cleanup_observation: Option<DescendantCleanupObservation>,
    /// Present only for a pre-started root: when its launch was handed to this
    /// scope. `observed_at_unix_ms` remains the actual creation time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessStartSnapshot {
    pub schema_version: u16,
    /// Preserves requested provenance when an opaque backend has no native observer.
    pub unobserved_launch_environment: Option<crate::environment_receipt::LaunchEnvironment>,
    pub requested_root_starts_observed: u64,
    pub auxiliary_root_starts_observed: u64,
    pub dropped_details: u64,
    pub roots: Vec<ProcessStart>,
    pub shared_instance_references: Vec<ProcessStart>,
    /// None means unknown, not zero. No OS-wide census is performed.
    pub descendant_starts: Option<u64>,
    pub requested_root_coverage: String,
    pub auxiliary_root_coverage: String,
}

#[derive(Debug, Default)]
pub struct ProcessStartRecorder {
    requested: AtomicU64,
    auxiliary: AtomicU64,
    dropped: AtomicU64,
    details: Mutex<(Vec<ProcessStart>, Vec<ProcessStart>)>,
}

impl ProcessStartRecorder {
    pub fn snapshot(&self) -> ProcessStartSnapshot {
        let details = self.details.try_lock().ok();
        ProcessStartSnapshot {
            schema_version: 2,
            unobserved_launch_environment: None,
            requested_root_starts_observed: self.requested.load(Ordering::Acquire),
            auxiliary_root_starts_observed: self.auxiliary.load(Ordering::Acquire),
            dropped_details: self.dropped.load(Ordering::Acquire) + u64::from(details.is_none()),
            roots: details.as_ref().map(|d| d.0.clone()).unwrap_or_default(),
            shared_instance_references: details.as_ref().map(|d| d.1.clone()).unwrap_or_default(),
            descendant_starts: None,
            requested_root_coverage:
                "instrumented_boundaries_only; late creation may outlive this snapshot".into(),
            auxiliary_root_coverage:
                "instrumented_boundaries_only; uninstrumented producers unknown".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessObservationContext {
    recorder: Arc<ProcessStartRecorder>,
    execution_id: String,
    environment: Option<crate::environment_receipt::LaunchEnvironment>,
}

tokio::task_local! { static ACTIVE: ProcessObservationContext; }
thread_local! { static BLOCKING: RefCell<Option<ProcessObservationContext>> = const { RefCell::new(None) }; }

impl ProcessObservationContext {
    pub fn new(recorder: Arc<ProcessStartRecorder>, execution_id: String) -> Self {
        Self {
            recorder,
            execution_id,
            environment: None,
        }
    }
    pub fn current() -> Option<Self> {
        ACTIVE
            .try_with(Clone::clone)
            .ok()
            .or_else(|| BLOCKING.with(|context| context.borrow().clone()))
    }
    pub async fn scope<F: Future>(self, future: F) -> F::Output {
        ACTIVE.scope(self, future).await
    }
    pub fn snapshot(&self) -> ProcessStartSnapshot {
        let mut snapshot = self.recorder.snapshot();
        if snapshot.roots.iter().all(|root| root.environment.is_none()) {
            snapshot.unobserved_launch_environment = self.environment.clone();
        }
        snapshot
    }
    pub async fn scope_environment<F: Future>(
        environment: crate::environment_receipt::LaunchEnvironment,
        future: F,
    ) -> F::Output {
        match Self::current() {
            Some(mut context) => {
                context.environment = Some(environment);
                context.scope(future).await
            }
            None => future.await,
        }
    }
    pub fn scope_sync<T>(context: Option<Self>, operation: impl FnOnce() -> T) -> T {
        struct Restore(Option<ProcessObservationContext>);
        impl Drop for Restore {
            fn drop(&mut self) {
                BLOCKING.with(|context| context.replace(self.0.take()));
            }
        }
        let _restore = Restore(BLOCKING.with(|slot| slot.replace(context)));
        operation()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ObservedProcess {
    instance_id: String,
    /// Shared by clones so a pre-started root adopted by a later scope routes
    /// its cleanup evidence to that scope.
    recorder: Arc<Mutex<Weak<ProcessStartRecorder>>>,
    creation: Option<Arc<ProcessStart>>,
}

impl ObservedProcess {
    /// Attribute a pre-started root, created outside any scope, to the current
    /// scope when its launch is handed to that scope's call: one start of its
    /// role with this scope's environment, executable identity and execution
    /// ID, the original creation time and the hand-off time. Later cleanup
    /// evidence is recorded in this scope. No-op without a creation record,
    /// without a current scope, or when the root was already observed by one.
    pub fn adopt_into_current_scope(&self) {
        let (Some(creation), Some(context)) =
            (self.creation.as_ref(), ProcessObservationContext::current())
        else {
            return;
        };
        let Ok(mut binding) = self.recorder.lock() else {
            context.recorder.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if binding.upgrade().is_some() {
            return;
        }
        let mut start = (**creation).clone();
        start.environment = context
            .environment
            .clone()
            .filter(|_| start.role == ProcessRole::RequestedRoot);
        start.resolved_executable = start
            .environment
            .as_ref()
            .and(start.os_pid)
            .and_then(crate::environment_receipt::executable_for_pid);
        start.executable_identity = crate::environment_receipt::ExecutableIdentity::observed(
            start.resolved_executable.as_deref(),
            start.environment.as_ref(),
        );
        start.execution_id = Some(context.execution_id.clone());
        start.handoff_at_unix_ms = Some(unix_ms());
        match start.role {
            ProcessRole::RequestedRoot => &context.recorder.requested,
            ProcessRole::AuxiliaryRoot => &context.recorder.auxiliary,
        }
        .fetch_add(1, Ordering::Release);
        if let Ok(mut details) = context.recorder.details.try_lock()
            && details.0.len() < DETAIL_LIMIT
        {
            details.0.push(start);
        } else {
            context.recorder.dropped.fetch_add(1, Ordering::Relaxed);
        }
        *binding = Arc::downgrade(&context.recorder);
    }

    /// Shared service references are not process births.
    pub fn reference(&self) {
        let Some(creation) = self.creation.as_ref() else {
            return;
        };
        if let Some(context) = ProcessObservationContext::current() {
            if let Ok(mut details) = context.recorder.details.try_lock() {
                if details
                    .1
                    .iter()
                    .any(|reference| reference.instance_id == self.instance_id)
                {
                    return;
                }
                if details.1.len() < DETAIL_LIMIT {
                    details.1.push((**creation).clone());
                } else {
                    context.recorder.dropped.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                context.recorder.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    /// Separate evidence: dropping a handle is not proof of cleanup/reaping.
    pub fn cleanup_observed(&self, outcome: &'static str) {
        self.update_start(|start| start.cleanup_observation = Some(outcome.into()));
    }

    #[cfg(windows)]
    pub(crate) fn contained_windows_job_emptied(&self) {
        self.update_start(|start| {
            start.descendant_cleanup_observation =
                Some(DescendantCleanupObservation::ContainedWindowsJobEmpty);
        });
    }

    fn update_start(&self, update: impl FnOnce(&mut ProcessStart)) {
        let recorder = self
            .recorder
            .lock()
            .ok()
            .and_then(|recorder| recorder.upgrade());
        if let Some(recorder) = recorder {
            if let Ok(mut details) = recorder.details.try_lock() {
                if let Some(start) = details
                    .0
                    .iter_mut()
                    .find(|start| start.instance_id == self.instance_id)
                {
                    update(start);
                }
            } else {
                recorder.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Call immediately after successful OS creation/remote acknowledgment, before
/// containment or pipe setup. Failed creation, polls and reuse must not call it.
pub fn observe_process_start(
    producer: &'static str,
    role: ProcessRole,
    os_pid: Option<u32>,
    backend_identity: Option<String>,
) -> ObservedProcess {
    let context = ProcessObservationContext::current();
    let instance_id = format!(
        "{}-{}",
        *RECORDER_ID,
        NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
    );
    let environment = context.as_ref().filter(|_| role == ProcessRole::RequestedRoot)
        .and_then(|context| context.environment.clone());
    let resolved_executable = context.as_ref().filter(|context| context.environment.is_some())
        .and_then(|_| os_pid.and_then(crate::environment_receipt::executable_for_pid));
    let start = ProcessStart {
        executable_identity: crate::environment_receipt::ExecutableIdentity::observed(
            resolved_executable.as_deref(), environment.as_ref(),
        ),
        resolved_executable,
        environment,
        instance_id: instance_id.clone(),
        execution_id: context.as_ref().map(|c| c.execution_id.clone()),
        producer: producer.into(),
        role,
        os_pid,
        provenance: if backend_identity.is_some() {
            "backend_acknowledgment"
        } else {
            "local_os_creation"
        }
        .into(),
        backend_identity,
        observed_at_unix_ms: unix_ms(),
        cleanup_observation: None,
        descendant_cleanup_observation: None,
        handoff_at_unix_ms: None,
    };
    // Keep birth/cleanup observation memory-only: logging or filesystem I/O at
    // this boundary must not delay containment, cleanup, or process delivery.
    let creation = Some(Arc::new(start.clone()));
    let recorder = Arc::new(Mutex::new(
        context
            .as_ref()
            .map(|c| Arc::downgrade(&c.recorder))
            .unwrap_or_default(),
    ));
    if let Some(context) = context {
        match role {
            ProcessRole::RequestedRoot => &context.recorder.requested,
            ProcessRole::AuxiliaryRoot => &context.recorder.auxiliary,
        }
        .fetch_add(1, Ordering::Release);
        if let Ok(mut details) = context.recorder.details.try_lock()
            && details.0.len() < DETAIL_LIMIT
        {
            details.0.push(start);
        } else {
            context.recorder.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    ObservedProcess {
        instance_id,
        recorder,
        creation,
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

