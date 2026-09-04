use super::procthreadattr::ProcThreadAttributeList;
use filedescriptor::FileDescriptor;
use filedescriptor::OwnedHandle;
use filedescriptor::Pipe;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::RawHandle;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use winapi::shared::minwindef::FALSE;
use winapi::shared::minwindef::TRUE;
use winapi::um::handleapi::GetHandleInformation;
use winapi::um::handleapi::SetHandleInformation;
use winapi::um::jobapi2::AssignProcessToJobObject;
use winapi::um::jobapi2::CreateJobObjectW;
use winapi::um::jobapi2::QueryInformationJobObject;
use winapi::um::jobapi2::SetInformationJobObject;
use winapi::um::jobapi2::TerminateJobObject;
use winapi::um::processthreadsapi::CreateProcessW;
use winapi::um::processthreadsapi::PROCESS_INFORMATION;
use winapi::um::winbase::CREATE_SUSPENDED;
use winapi::um::winbase::CREATE_UNICODE_ENVIRONMENT;
use winapi::um::winbase::EXTENDED_STARTUPINFO_PRESENT;
use winapi::um::winbase::HANDLE_FLAG_INHERIT;
use winapi::um::winbase::HANDLE_FLAG_PROTECT_FROM_CLOSE;
use winapi::um::winbase::STARTF_USESTDHANDLES;
use winapi::um::winbase::STARTUPINFOEXW;
use winapi::um::winnt::JOB_OBJECT_LIMIT_BREAKAWAY_OK;
use winapi::um::winnt::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use winapi::um::winnt::JOBOBJECT_BASIC_ACCOUNTING_INFORMATION;
use winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use winapi::um::winnt::JobObjectBasicAccountingInformation;
use winapi::um::winnt::JobObjectExtendedLimitInformation;

/// Owns a Windows Job Object used to terminate a spawned process tree.
#[derive(Debug)]
pub struct JobObject {
    handle: OwnedHandle,
    allow_descendant_preservation: bool,
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

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CompareStringOrdinal(
        string1: *const u16,
        string1_len: i32,
        string2: *const u16,
        string2_len: i32,
        ignore_case: i32,
    ) -> i32;
}

const CSTR_LESS_THAN: i32 = 1;
const CSTR_EQUAL: i32 = 2;
const CSTR_GREATER_THAN: i32 = 3;
const CREATED_PROCESS_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct CreatedSuspendedProcess {
    pub(crate) process: OwnedHandle,
    pub(crate) primary_thread: OwnedHandle,
    pub(crate) pid: u32,
    pub(crate) stdin: FileDescriptor,
    pub(crate) stdout: FileDescriptor,
    pub(crate) stderr: FileDescriptor,
}

enum CoordinatedCreateResult {
    Created(CreatedSuspendedProcess),
    CleanupCreatedProcess {
        process: OwnedHandle,
        error: io::Error,
    },
}

impl JobObject {
    /// Creates a Job Object configured to terminate all members when its last handle closes.
    pub fn create() -> io::Result<Self> {
        Self::create_with_limit_flags(
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK,
            true,
        )
    }

    /// Creates a Job Object whose members cannot explicitly break descendants away.
    pub(crate) fn create_strict() -> io::Result<Self> {
        Self::create_with_limit_flags(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, false)
    }

    fn create_with_limit_flags(
        limit_flags: u32,
        allow_descendant_preservation: bool,
    ) -> io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };

        Self::set_limit_flags(&handle, limit_flags)?;

        Ok(Self {
            handle,
            allow_descendant_preservation,
            state: Mutex::new(JobState::Active),
        })
    }

    fn set_limit_flags(handle: &OwnedHandle, flags: u32) -> io::Result<()> {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = flags;
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
        let assigned = unsafe {
            AssignProcessToJobObject(self.handle.as_raw_handle().cast(), process_handle.cast())
        };
        if assigned == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Allows contained descendants to keep running after the root exits normally.
    ///
    /// This disables both explicit job termination and kill-on-close for this
    /// object. Calls race safely with [`Self::terminate`]: whichever operation
    /// acquires the state lock first determines whether the process tree is
    /// preserved or terminated.
    pub fn preserve_descendants(&self) -> io::Result<()> {
        if !self.allow_descendant_preservation {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "strict Windows Job containment cannot be relaxed",
            ));
        }
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
            JobState::PreserveDescendants => return Ok(()),
            JobState::Active | JobState::TerminationRequested | JobState::Terminated => {
                *state = JobState::TerminationRequested;
            }
        }

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

    /// Returns the number of processes that are still active in this job.
    pub(crate) fn active_process_count(&self) -> io::Result<u32> {
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        let queried = unsafe {
            QueryInformationJobObject(
                self.handle.as_raw_handle().cast(),
                JobObjectBasicAccountingInformation,
                std::ptr::addr_of_mut!(accounting).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(accounting.ActiveProcesses)
        }
    }

    /// Creates a suspended process whose Job membership and inherited handle
    /// allow-list are applied atomically by the kernel.
    pub(crate) fn create_suspended_with_pipes(
        &self,
        executable: &OsStr,
        args: &[OsString],
        cwd: &Path,
        env: &BTreeMap<OsString, OsString>,
        inject_inherit_reset_failure: bool,
    ) -> io::Result<CreatedSuspendedProcess> {
        let prepared = PreparedLaunch::new(executable, args, cwd, env)?;
        let coordinated = crate::with_windows_child_creation(move |scope| {
            let stdin = Pipe::new().map_err(io::Error::other)?;
            let stdout = Pipe::new().map_err(io::Error::other)?;
            let stderr = Pipe::new().map_err(io::Error::other)?;
            let parent_handles = [
                stdin.write.as_raw_handle().cast(),
                stdout.read.as_raw_handle().cast(),
                stderr.read.as_raw_handle().cast(),
            ];
            for &handle in &parent_handles {
                set_handle_inherit(handle, false)?;
            }
            let child_handles = [
                stdin.read.as_raw_handle().cast(),
                stdout.write.as_raw_handle().cast(),
                stderr.write.as_raw_handle().cast(),
            ];

            // PROC_THREAD_ATTRIBUTE_HANDLE_LIST still requires its entries to
            // carry HANDLE_FLAG_INHERIT. Preserve each distinct handle's exact
            // flags while that short process-global exposure is serialized.
            let mut inherit_reset = InheritFlagReset::new(scope);
            for &handle in &child_handles {
                if let Err(primary) = inherit_reset.make_inheritable(handle) {
                    let error = combine_optional_error(primary, inherit_reset.restore().err());
                    drop(stdin.read);
                    drop(stdout.write);
                    drop(stderr.write);
                    return Err(error);
                }
            }

            #[cfg(test)]
            crate::windows_child_creation::emit_strict_handle_test_event(
                crate::windows_child_creation::WindowsStrictHandleEvent::Inheritable(
                    child_handles.map(|handle| handle as usize),
                ),
            );

            let result = self.create_suspended_with_pipes_inner(prepared, child_handles);
            let reset_error = inherit_reset.restore().err();
            #[cfg(test)]
            if reset_error.is_none() {
                crate::windows_child_creation::emit_strict_handle_test_event(
                    crate::windows_child_creation::WindowsStrictHandleEvent::Restored {
                        handles: child_handles.map(|handle| handle as usize),
                        flags: inherit_reset.original_flags_for(child_handles),
                    },
                );
            }
            let injected_reset_error = inject_inherit_reset_failure.then(|| {
                scope.mark_inheritance_state_compromised();
                io::Error::other("injected post-CreateProcessW inheritance-reset failure")
            });
            let post_creation_error = reset_error.or(injected_reset_error);

            // These are the temporarily exposed owners. Close them before the
            // coordinator unlocks on every success, error, and unwind path.
            drop(stdin.read);
            drop(stdout.write);
            drop(stderr.write);

            let (process, primary_thread, pid) = match result {
                Ok(created) => created,
                Err(primary) => {
                    return Err(combine_optional_error(primary, post_creation_error));
                }
            };
            if let Some(error) = post_creation_error {
                return Ok(CoordinatedCreateResult::CleanupCreatedProcess { process, error });
            }
            Ok(CoordinatedCreateResult::Created(CreatedSuspendedProcess {
                process,
                primary_thread,
                pid,
                stdin: stdin.write,
                stdout: stdout.read,
                stderr: stderr.read,
            }))
        })?;

        match coordinated {
            CoordinatedCreateResult::Created(created) => Ok(created),
            CoordinatedCreateResult::CleanupCreatedProcess { process, error } => {
                Err(self.cleanup_created_process_error(&process, error))
            }
        }
    }

    pub(crate) fn cleanup_created_process_error(
        &self,
        process: &OwnedHandle,
        primary: io::Error,
    ) -> io::Error {
        match self.terminate_reap_and_observe_empty(process) {
            Ok(()) => primary,
            Err(cleanup) => io::Error::other(format!(
                "{primary}; contained child cleanup also failed: {cleanup}"
            )),
        }
    }

    pub(crate) fn terminate_reap_and_observe_empty(&self, process: &OwnedHandle) -> io::Result<()> {
        use winapi::um::processthreadsapi::TerminateProcess;
        use winapi::um::synchapi::WaitForSingleObject;
        use winapi::um::winbase::WAIT_OBJECT_0;

        if let Err(job_error) = self.terminate()
            && unsafe { TerminateProcess(process.as_raw_handle().cast(), 1) } == 0
        {
            return Err(io::Error::other(format!(
                "failed to terminate Windows Job ({job_error}); root fallback also failed: {}",
                io::Error::last_os_error()
            )));
        }
        let deadline = std::time::Instant::now() + CREATED_PROCESS_CLEANUP_TIMEOUT;
        let timeout_ms =
            u32::try_from(CREATED_PROCESS_CLEANUP_TIMEOUT.as_millis()).unwrap_or(u32::MAX - 1);
        let waited = unsafe { WaitForSingleObject(process.as_raw_handle().cast(), timeout_ms) };
        if waited != WAIT_OBJECT_0 {
            return if waited == u32::MAX {
                Err(io::Error::last_os_error())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "contained child did not exit after Job termination",
                ))
            };
        }
        loop {
            let active = self.active_process_count()?;
            if active == 0 {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("Job still contains {active} process(es) after termination"),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn create_suspended_with_pipes_inner(
        &self,
        mut prepared: PreparedLaunch,
        child_handles: [winapi::um::winnt::HANDLE; 3],
    ) -> io::Result<(OwnedHandle, OwnedHandle, u32)> {
        let mut attrs = ProcThreadAttributeList::with_capacity(2).map_err(io::Error::other)?;
        attrs
            .set_job(self.handle.as_raw_handle().cast())
            .map_err(io::Error::other)?;
        attrs
            .set_handle_list(child_handles.to_vec())
            .map_err(io::Error::other)?;

        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = child_handles[0];
        startup.StartupInfo.hStdOutput = child_handles[1];
        startup.StartupInfo.hStdError = child_handles[2];
        startup.lpAttributeList = attrs.as_mut_ptr();

        let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let created = unsafe {
            CreateProcessW(
                prepared.executable.as_ptr(),
                prepared.command_line.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                TRUE,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                prepared.environment.as_mut_ptr().cast(),
                prepared.cwd.as_ptr(),
                &mut startup.StartupInfo,
                &mut process_info,
            )
        };
        if created == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe {
            (
                OwnedHandle::from_raw_handle(process_info.hProcess.cast()),
                OwnedHandle::from_raw_handle(process_info.hThread.cast()),
                process_info.dwProcessId,
            )
        })
    }
}

struct PreparedLaunch {
    executable: Vec<u16>,
    command_line: Vec<u16>,
    cwd: Vec<u16>,
    environment: Vec<u16>,
}

impl PreparedLaunch {
    fn new(
        executable: &OsStr,
        args: &[OsString],
        cwd: &Path,
        env: &BTreeMap<OsString, OsString>,
    ) -> io::Result<Self> {
        let executable = encode_nul_terminated(executable, "executable")?;
        let cwd = encode_nul_terminated(cwd.as_os_str(), "working directory")?;
        Ok(Self {
            command_line: build_command_line_from_encoded_executable(&executable, args)?,
            executable,
            cwd,
            environment: build_environment_block(env)?,
        })
    }
}

fn encode_nul_terminated(value: &OsStr, label: &str) -> io::Result<Vec<u16>> {
    let mut encoded = value.encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} contains an embedded NUL"),
        ));
    }
    encoded.push(0);
    Ok(encoded)
}

struct InheritFlagState {
    handle: winapi::um::winnt::HANDLE,
    original_flags: u32,
}

struct InheritFlagReset<'a> {
    scope: &'a crate::WindowsChildCreationScope,
    states: Vec<InheritFlagState>,
    restored: bool,
}

impl<'a> InheritFlagReset<'a> {
    fn new(scope: &'a crate::WindowsChildCreationScope) -> Self {
        Self {
            scope,
            states: Vec::new(),
            restored: false,
        }
    }

    fn make_inheritable(&mut self, handle: winapi::um::winnt::HANDLE) -> io::Result<()> {
        if self.states.iter().any(|state| state.handle == handle) {
            return Ok(());
        }
        let original_flags = get_handle_flags(handle)?;
        self.states.push(InheritFlagState {
            handle,
            original_flags,
        });
        if original_flags & HANDLE_FLAG_INHERIT == 0 {
            set_handle_inherit(handle, true)?;
        }
        Ok(())
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        let mut first_error = None;
        for state in self.states.iter().rev() {
            let restored = (|| {
                let current_flags = get_handle_flags(state.handle)?;
                if current_flags != state.original_flags {
                    let restorable_flags = HANDLE_FLAG_INHERIT | HANDLE_FLAG_PROTECT_FROM_CLOSE;
                    if unsafe {
                        SetHandleInformation(
                            state.handle,
                            restorable_flags,
                            state.original_flags & restorable_flags,
                        )
                    } == 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                let restored_flags = get_handle_flags(state.handle)?;
                if restored_flags != state.original_flags {
                    return Err(io::Error::other(format!(
                        "Windows handle flags were not restored exactly: expected {:#x}, got {restored_flags:#x}",
                        state.original_flags
                    )));
                }
                Ok(())
            })();
            if let Err(error) = restored
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        self.restored = true;
        if first_error.is_some() {
            self.scope.mark_inheritance_state_compromised();
        }
        first_error.map_or(Ok(()), Err)
    }

    #[cfg(test)]
    fn original_flags_for(&self, handles: [winapi::um::winnt::HANDLE; 3]) -> [u32; 3] {
        handles.map(|handle| {
            self.states
                .iter()
                .find(|state| state.handle == handle)
                .expect("every strict child handle has preserved flags")
                .original_flags
        })
    }
}

impl Drop for InheritFlagReset<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn get_handle_flags(handle: winapi::um::winnt::HANDLE) -> io::Result<u32> {
    let mut flags = 0;
    if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags)
    }
}

fn set_handle_inherit(handle: winapi::um::winnt::HANDLE, inherit: bool) -> io::Result<()> {
    let current = get_handle_flags(handle)?;
    if (current & HANDLE_FLAG_INHERIT != 0) == inherit {
        return Ok(());
    }
    let flags = if inherit { HANDLE_FLAG_INHERIT } else { 0 };
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, flags) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn combine_errors(primary: io::Error, cleanup: io::Error) -> io::Error {
    io::Error::other(format!(
        "{primary}; exact Windows handle-flag restoration also failed: {cleanup}"
    ))
}

fn combine_optional_error(primary: io::Error, cleanup: Option<io::Error>) -> io::Error {
    match cleanup {
        Some(cleanup) => combine_errors(primary, cleanup),
        None => primary,
    }
}

fn build_environment_block(env: &BTreeMap<OsString, OsString>) -> io::Result<Vec<u16>> {
    struct Entry {
        key: Vec<u16>,
        value: Vec<u16>,
    }

    let mut entries: Vec<Entry> = Vec::with_capacity(env.len());
    for (key, value) in env {
        let key_wide = key.encode_wide().collect::<Vec<_>>();
        let value_wide = value.encode_wide().collect::<Vec<_>>();
        if key_wide.is_empty() || key_wide.contains(&0) || key_wide.contains(&('=' as u16)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid Windows environment name {key:?}"),
            ));
        }
        if value_wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Windows environment value for {key:?} contains an embedded NUL"),
            ));
        }

        let mut insert_at = entries.len();
        for (index, existing) in entries.iter().enumerate() {
            match compare_ordinal_ignore_case(&key_wide, &existing.key)? {
                std::cmp::Ordering::Less => {
                    insert_at = index;
                    break;
                }
                std::cmp::Ordering::Equal => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("duplicate Windows environment name {key:?}"),
                    ));
                }
                std::cmp::Ordering::Greater => {}
            }
        }
        entries.insert(
            insert_at,
            Entry {
                key: key_wide,
                value: value_wide,
            },
        );
    }

    let mut block = Vec::new();
    for entry in entries {
        block.extend(entry.key);
        block.push('=' as u16);
        block.extend(entry.value);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn compare_ordinal_ignore_case(left: &[u16], right: &[u16]) -> io::Result<std::cmp::Ordering> {
    let left_len = i32::try_from(left.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "environment name is too long"))?;
    let right_len = i32::try_from(right.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "environment name is too long"))?;
    match unsafe { CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, TRUE) }
    {
        CSTR_LESS_THAN => Ok(std::cmp::Ordering::Less),
        CSTR_EQUAL => Ok(std::cmp::Ordering::Equal),
        CSTR_GREATER_THAN => Ok(std::cmp::Ordering::Greater),
        _ => Err(io::Error::last_os_error()),
    }
}

fn build_command_line_from_encoded_executable(
    executable_wide: &[u16],
    args: &[OsString],
) -> io::Result<Vec<u16>> {
    let executable_without_nul = &executable_wide[..executable_wide.len() - 1];
    use std::os::windows::ffi::OsStringExt;
    let executable = OsString::from_wide(executable_without_nul);
    let mut command_line = Vec::new();
    append_quoted(&executable, &mut command_line);
    let raw_payload = crate::windows_cmd_payload_index(&executable, args);
    for (index, arg) in args.iter().enumerate() {
        if arg.encode_wide().any(|unit| unit == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "command argument contains an embedded NUL",
            ));
        }
        command_line.push(' ' as u16);
        if raw_payload == Some(index) {
            command_line.push('"' as u16);
            command_line.extend(arg.encode_wide());
            command_line.push('"' as u16);
        } else {
            append_quoted(arg, &mut command_line);
        }
    }
    command_line.push(0);
    Ok(command_line)
}

// Keep this byte-for-byte equivalent to the quoting used by the existing
// ConPTY launcher and Rust's MSVCRT-compatible command-line construction.
fn append_quoted(arg: &OsStr, command_line: &mut Vec<u16>) {
    if !arg.is_empty()
        && !arg.encode_wide().any(|unit| {
            unit == ' ' as u16
                || unit == '\t' as u16
                || unit == '\n' as u16
                || unit == '\x0b' as u16
                || unit == '"' as u16
        })
    {
        command_line.extend(arg.encode_wide());
        return;
    }
    command_line.push('"' as u16);
    let arg = arg.encode_wide().collect::<Vec<_>>();
    let mut index = 0;
    while index < arg.len() {
        let mut backslashes = 0;
        while index < arg.len() && arg[index] == '\\' as u16 {
            index += 1;
            backslashes += 1;
        }
        if index == arg.len() {
            command_line.extend(std::iter::repeat_n('\\' as u16, backslashes * 2));
            break;
        }
        if arg[index] == '"' as u16 {
            command_line.extend(std::iter::repeat_n('\\' as u16, backslashes * 2 + 1));
        } else {
            command_line.extend(std::iter::repeat_n('\\' as u16, backslashes));
        }
        command_line.push(arg[index]);
        index += 1;
    }
    command_line.push('"' as u16);
}

impl AsRawHandle for JobObject {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}

#[cfg(test)]
mod tests {
    use super::JobObject;
    use super::JobState;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::time::Duration;
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::winbase::WAIT_OBJECT_0;

    #[test]
    fn termination_cannot_be_overridden_by_later_preservation() -> io::Result<()> {
        let job = JobObject::create()?;

        job.terminate()?;
        job.preserve_descendants()?;

        assert_eq!(*job.state.lock().unwrap(), JobState::Terminated);
        Ok(())
    }

    #[test]
    fn strict_job_rejects_descendant_preservation() -> io::Result<()> {
        let job = JobObject::create_strict()?;

        let error = job
            .preserve_descendants()
            .expect_err("strict containment must not be relaxed");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(*job.state.lock().unwrap(), JobState::Active);
        Ok(())
    }

    #[test]
    fn repeated_termination_kills_a_process_assigned_after_the_first_call() -> io::Result<()> {
        let job = JobObject::create_strict()?;
        job.terminate()?;

        let marker = std::env::temp_dir().join(format!(
            "kd4-job-late-assignment-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&marker);
        let executable = std::env::var_os("ComSpec")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));
        let payload = format!("echo started>\"{}\"", marker.display());
        let created = job.create_suspended_with_pipes(
            executable.as_os_str(),
            &[
                OsString::from("/d"),
                OsString::from("/s"),
                OsString::from("/c"),
                OsString::from(payload),
            ],
            &std::env::current_dir()?,
            &BTreeMap::new(),
            false,
        )?;

        job.terminate()?;
        let waited = unsafe {
            WaitForSingleObject(
                created.process.as_raw_handle().cast(),
                u32::try_from(Duration::from_secs(2).as_millis()).unwrap(),
            )
        };
        assert_eq!(waited, WAIT_OBJECT_0);
        assert_eq!(job.active_process_count()?, 0);
        assert!(!marker.exists(), "suspended process must never execute");
        Ok(())
    }
}
