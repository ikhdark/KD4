use filedescriptor::OwnedHandle;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::RawHandle;
use std::sync::Mutex;
use winapi::um::jobapi2::AssignProcessToJobObject;
use winapi::um::jobapi2::CreateJobObjectW;
use winapi::um::jobapi2::QueryInformationJobObject;
use winapi::um::jobapi2::SetInformationJobObject;
use winapi::um::jobapi2::TerminateJobObject;
use winapi::um::winnt::JOB_OBJECT_LIMIT_BREAKAWAY_OK;
use winapi::um::winnt::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use winapi::um::winnt::JOBOBJECT_BASIC_ACCOUNTING_INFORMATION;
use winapi::um::winnt::JobObjectBasicAccountingInformation;
use winapi::um::winnt::JobObjectExtendedLimitInformation;

/// Owns a Windows Job Object used to terminate a spawned process tree.
#[derive(Debug)]
pub struct JobObject {
    handle: OwnedHandle,
    // A mutex makes the state check, Job Object API call, and state update
    // atomic with respect to concurrent preserve and terminate requests.
    state: Mutex<JobState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobState {
    Active,
    PreserveDescendants,
    TerminationRequested,
    Terminated,
}

impl JobObject {
    fn custody_diagnostic(&self, phase: &str, flags: u32) {
        if std::env::var("CODEX_SANDBOX_CUSTODY_DIAGNOSTICS").ok().as_deref() != Some("1") {
            return;
        }
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: This owner retains the job and the exact accounting output.
        let queried = unsafe { QueryInformationJobObject(
            self.handle.as_raw_handle().cast(), JobObjectBasicAccountingInformation,
            std::ptr::addr_of_mut!(accounting).cast(),
            std::mem::size_of_val(&accounting) as u32, std::ptr::null_mut(),
        ) };
        static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let elapsed = ORIGIN.get_or_init(std::time::Instant::now).elapsed().as_micros();
        eprintln!("job_custody observer_pid={} t_us={} job={} phase={} flags={} query={} active={}",
            std::process::id(), elapsed, self.handle.as_raw_handle() as usize, phase, flags, queried, accounting.ActiveProcesses);
    }

    /// Creates a Job Object configured to terminate all members when its last handle closes.
    pub fn create() -> io::Result<Self> {
        // SAFETY: Both null arguments request the default security descriptor and an unnamed
        // job; the returned handle is checked before use.
        let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW succeeded and returned this uniquely owned handle;
        // OwnedHandle closes it once.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };

        Self::set_limit_flags(
            &handle,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        )?;

        Ok(Self {
            handle,
            state: Mutex::new(JobState::Active),
        })
    }

    fn set_limit_flags(handle: &OwnedHandle, flags: u32) -> io::Result<()> {
        // SAFETY: JOBOBJECT_EXTENDED_LIMIT_INFORMATION contains integer and pointer fields that
        // all admit the zero representation.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = flags;
        // SAFETY: handle owns the job; limits is initialized and remains writable for the exact
        // structure size passed to Windows.
        let configured = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle().cast(),
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of_mut!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }


    /// Assigns a running process to this job.
    ///
    /// Assignment is not retroactive: descendants created before this call
    /// completes are not guaranteed to become members of the job.
    pub fn assign_process(&self, process_handle: RawHandle) -> io::Result<()> {
        // SAFETY: The owned job handle remains live for this call. Windows validates the opaque
        // process handle; it is not dereferenced as a Rust pointer.
        let assigned = unsafe {
            AssignProcessToJobObject(self.handle.as_raw_handle().cast(), process_handle.cast())
        };
        if assigned == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Keep descendants in this job even when a launcher requests breakaway.
    /// Call before assigning the root process.
    pub fn require_descendant_containment(&self) -> io::Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("job state lock poisoned"))?;
        if *state != JobState::Active {
            return Err(io::Error::other("job containment is no longer active"));
        }
        Self::set_limit_flags(&self.handle, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)?;
        self.custody_diagnostic("contained", JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE);
        Ok(())
    }

    /// Allows contained descendants to keep running after the root exits normally.
    ///
    /// This disables both explicit job termination and kill-on-close for this
    /// object. Calls race safely with [`Self::terminate`]: whichever operation
    /// acquires the state lock first determines whether the process tree is
    /// preserved or terminated.
    pub fn preserve_descendants(&self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("job state lock poisoned"))?;
        match *state {
            JobState::Active => {}
            JobState::PreserveDescendants
            | JobState::TerminationRequested
            | JobState::Terminated => return Ok(()),
        }

        Self::set_limit_flags(&self.handle, JOB_OBJECT_LIMIT_BREAKAWAY_OK)?;
        self.custody_diagnostic("preserved", JOB_OBJECT_LIMIT_BREAKAWAY_OK);
        *state = JobState::PreserveDescendants;
        Ok(())
    }

    /// Terminates every process currently assigned to the job.
    pub fn terminate(&self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("job state lock poisoned"))?;
        match *state {
            JobState::PreserveDescendants | JobState::Terminated => return Ok(()),
            JobState::Active => *state = JobState::TerminationRequested,
            JobState::TerminationRequested => {}
        }

        // SAFETY: self.handle owns the live job throughout the call; the exit code is a scalar
        // and the state mutex serializes transitions.
        self.custody_diagnostic("terminate_requested", 0);
        let terminated = unsafe {
            TerminateJobObject(self.handle.as_raw_handle().cast(), /*uExitCode*/ 1)
        };
        if terminated == 0 {
            Err(io::Error::last_os_error())
        } else {
            *state = JobState::Terminated;
            Ok(())
        }
    }
}

impl AsRawHandle for JobObject {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}

