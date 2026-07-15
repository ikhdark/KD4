use crate::model::CommandArgV2;
use crate::model::CommandResultV2;
use crate::model::CommandSpecV2;
use crate::model::LaunchErrorKind;
use crate::model::LogState;
use crate::model::PlanEnvelopeV2;
use crate::model::VERIFY_LOCAL_V2_SCHEMA_VERSION;
use crate::secure_result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const MAX_FRAME_PAYLOAD: usize = 16 * 1024;
const COLLECTOR_CHANNEL_FRAMES: usize = 64;
const GRACEFUL_TERMINATION: Duration = Duration::from_secs(5);
const POST_TERMINATION_DRAIN: Duration = Duration::from_secs(2);

pub fn execute_plan(plan: &PlanEnvelopeV2, repository_root: &Path) -> Vec<CommandResultV2> {
    let invocation_nonce = secure_result::random_hex_128().unwrap_or_else(|error| {
        let digest = Sha256::digest(error.as_bytes());
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    });
    let result_dir = secure_result::create_invocation_dir(
        repository_root,
        &plan.invocation_id,
        &invocation_nonce,
    );
    plan.commands
        .iter()
        .enumerate()
        .map(|(ordinal, command)| {
            execute_command(
                plan,
                command,
                ordinal,
                repository_root,
                result_dir.as_ref().ok().map(PathBuf::as_path),
            )
        })
        .collect()
}

fn execute_command(
    plan: &PlanEnvelopeV2,
    command: &CommandSpecV2,
    ordinal: usize,
    repository_root: &Path,
    result_dir: Option<&Path>,
) -> CommandResultV2 {
    let nonce = secure_result::random_hex_128().unwrap_or_else(|error| {
        let digest = Sha256::digest(error.as_bytes());
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    });
    let mut result = base_result(plan, command, ordinal, nonce);
    if result_dir.is_none() {
        result.runner_error = Some("failed to allocate private result directory".to_string());
        result.log_state = LogState::IntegrityFailure;
        return result;
    }
    let (program, arguments) = match command.args.split_first() {
        Some((program, arguments)) => (program, arguments),
        None => {
            result.runner_error = Some("planned command has no executable".to_string());
            result.launch_error = Some(LaunchErrorKind::Other);
            return persist_result(result_dir, result);
        }
    };
    let program = match argument_to_os_string(program) {
        Ok(program) => program,
        Err(error) => {
            result.runner_error = Some(error);
            result.launch_error = Some(LaunchErrorKind::UnsupportedPath);
            return persist_result(result_dir, result);
        }
    };
    let arguments = match arguments
        .iter()
        .map(argument_to_os_string)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(arguments) => arguments,
        Err(error) => {
            result.runner_error = Some(error);
            result.launch_error = Some(LaunchErrorKind::UnsupportedPath);
            return persist_result(result_dir, result);
        }
    };
    let cwd = match path_to_path_buf(&command.cwd) {
        Ok(cwd) => cwd,
        Err(error) => {
            result.runner_error = Some(error);
            result.launch_error = Some(LaunchErrorKind::UnsupportedPath);
            return persist_result(result_dir, result);
        }
    };

    let started = Instant::now();
    let log = match open_framed_log(repository_root, command, ordinal, &result) {
        Ok(log) => log,
        Err(error) => {
            result.runner_error = Some(error);
            result.log_state = LogState::IoFailure;
            return persist_result(result_dir, result);
        }
    };
    result.log_path = Some(log.path.clone());

    let mut process = Command::new(&program);
    process
        .args(&arguments)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, process_tree) = match spawn_process_tree(&mut process) {
        Ok(spawned) => spawned,
        Err(error) => {
            result.duration_ns = nanos_u64(started.elapsed().as_nanos());
            result.runner_error = Some(error.to_string());
            result.launch_error = Some(match error.kind() {
                std::io::ErrorKind::NotFound => LaunchErrorKind::CommandNotFound,
                std::io::ErrorKind::PermissionDenied => LaunchErrorKind::PermissionDenied,
                _ => LaunchErrorKind::Other,
            });
            return persist_result(result_dir, result);
        }
    };

    let (frame_tx, frame_rx) = mpsc::sync_channel(COLLECTOR_CHANNEL_FRAMES);
    let (done_tx, done_rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        spawn_reader("stdout", stdout, frame_tx.clone(), done_tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader("stderr", stderr, frame_tx.clone(), done_tx.clone());
    }
    drop(frame_tx);
    drop(done_tx);

    let mut collector = FrameCollector::new(log.file);
    let timeout = Duration::from_millis(command.timeout_ms.max(1));
    let mut exit_status = None;
    let mut logging_fault = None;
    loop {
        while let Ok(frame) = frame_rx.try_recv() {
            if let Err(error) = collector.write_frame(&frame) {
                logging_fault = Some(error);
                terminate_process_tree(&mut child, &process_tree);
                break;
            }
        }
        if logging_fault.is_some() {
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break;
            }
            Ok(None) if started.elapsed() >= timeout => {
                result.timed_out = true;
                terminate_process_tree(&mut child, &process_tree);
                result.log_state = LogState::IncompleteAfterTermination;
                break;
            }
            Ok(None) => match frame_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(frame) => {
                    if let Err(error) = collector.write_frame(&frame) {
                        logging_fault = Some(error);
                        terminate_process_tree(&mut child, &process_tree);
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            },
            Err(error) => {
                result.runner_error = Some(error.to_string());
                terminate_process_tree(&mut child, &process_tree);
                break;
            }
        }
    }

    let completed_readers = drain_after_exit(&frame_rx, &done_rx, &mut collector);
    if completed_readers < 2 && logging_fault.is_none() && !result.timed_out {
        result.log_state = LogState::IncompleteAfterTermination;
    }
    result.duration_ns = nanos_u64(started.elapsed().as_nanos());
    if let Some(status) = exit_status {
        result.exit_code = status.code();
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            result.signal = status.signal();
        }
    }
    if let Some(error) = logging_fault {
        result.runner_error = Some(error);
        result.log_state = LogState::IoFailure;
    }
    if let Err(error) = collector.finish() {
        result.runner_error = Some(error);
        result.log_state = LogState::IoFailure;
    }
    result.diagnostic = collector.diagnostic.finish();
    persist_result(result_dir, result)
}

fn persist_result(result_dir: Option<&Path>, mut result: CommandResultV2) -> CommandResultV2 {
    let Some(result_dir) = result_dir else {
        result.log_state = LogState::IntegrityFailure;
        result.runner_error = Some("missing private result directory".to_string());
        return result;
    };
    match secure_result::write_result_file(result_dir, &result) {
        Ok(parsed) => parsed,
        Err(error) => {
            result.log_state = LogState::IntegrityFailure;
            result.runner_error = Some(error);
            result
        }
    }
}

fn base_result(
    plan: &PlanEnvelopeV2,
    command: &CommandSpecV2,
    ordinal: usize,
    nonce: String,
) -> CommandResultV2 {
    CommandResultV2 {
        schema_version: VERIFY_LOCAL_V2_SCHEMA_VERSION,
        invocation_id: plan.invocation_id.clone(),
        command_id: command.id.clone(),
        command_ordinal: ordinal,
        runner_nonce: nonce,
        exit_code: None,
        signal: None,
        duration_ns: 0,
        timed_out: false,
        cancelled: false,
        runner_error: None,
        launch_error: None,
        log_state: LogState::Complete,
        log_path: None,
        diagnostic: String::new(),
        cached: false,
        flaky: false,
        baseline: None,
    }
}

fn argument_to_os_string(argument: &CommandArgV2) -> Result<OsString, String> {
    match argument {
        CommandArgV2::Text { value } => Ok(OsString::from(value)),
        CommandArgV2::Path { path } => raw_path_to_os_string(path),
    }
}

#[cfg(unix)]
fn raw_path_to_os_string(path: &crate::model::RawPath) -> Result<OsString, String> {
    Ok(path.to_os_string())
}

#[cfg(windows)]
fn raw_path_to_os_string(path: &crate::model::RawPath) -> Result<OsString, String> {
    path.to_os_string()
}

fn path_to_path_buf(path: &crate::model::RawPath) -> Result<PathBuf, String> {
    raw_path_to_os_string(path).map(PathBuf::from)
}

struct FramedLog {
    path: PathBuf,
    file: File,
}

fn open_framed_log(
    repository_root: &Path,
    command: &CommandSpecV2,
    ordinal: usize,
    result: &CommandResultV2,
) -> Result<FramedLog, String> {
    let directory = repository_root.join(".codex/verify-local/logs");
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let path = directory.join(format!(
        "{ordinal:04}-{}-{}-{}.jsonl",
        secure_result::command_token(&command.id),
        result.invocation_id,
        result.runner_nonce
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
    }
    Ok(FramedLog { path, file })
}

#[derive(Serialize)]
struct LogFrame<'a> {
    seq: u64,
    stream: &'a str,
    monotonic_ns: u64,
    bytes_base64: String,
}

struct StreamFrame {
    stream: &'static str,
    monotonic_ns: u64,
    bytes: Vec<u8>,
}

struct FrameCollector {
    file: File,
    seq: u64,
    diagnostic: DiagnosticAccumulator,
}

impl FrameCollector {
    fn new(file: File) -> Self {
        Self {
            file,
            seq: 0,
            diagnostic: DiagnosticAccumulator::new(),
        }
    }

    fn write_frame(&mut self, frame: &StreamFrame) -> Result<(), String> {
        let log_frame = LogFrame {
            seq: self.seq,
            stream: frame.stream,
            monotonic_ns: frame.monotonic_ns,
            bytes_base64: BASE64_STANDARD.encode(&frame.bytes),
        };
        let mut line = serde_json::to_vec(&log_frame).map_err(|error| error.to_string())?;
        line.push(b'\n');
        self.file
            .write_all(&line)
            .map_err(|error| error.to_string())?;
        self.diagnostic.push(&line);
        self.seq += 1;
        Ok(())
    }

    fn finish(&mut self) -> Result<(), String> {
        self.file.flush().map_err(|error| error.to_string())?;
        self.file.sync_all().map_err(|error| error.to_string())
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    stream: &'static str,
    mut reader: R,
    sender: mpsc::SyncSender<StreamFrame>,
    done: mpsc::Sender<()>,
) {
    thread::spawn(move || {
        let started = Instant::now();
        let mut buffer = vec![0_u8; MAX_FRAME_PAYLOAD];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let frame = StreamFrame {
                        stream,
                        monotonic_ns: nanos_u64(started.elapsed().as_nanos()),
                        bytes: buffer[..count].to_vec(),
                    };
                    if sender.send(frame).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = done.send(());
    });
}

fn drain_after_exit(
    receiver: &mpsc::Receiver<StreamFrame>,
    done: &mpsc::Receiver<()>,
    collector: &mut FrameCollector,
) -> usize {
    let deadline = Instant::now() + POST_TERMINATION_DRAIN;
    let mut completed = 0;
    while completed < 2 && Instant::now() < deadline {
        while let Ok(frame) = receiver.try_recv() {
            let _ = collector.write_frame(&frame);
        }
        match done.recv_timeout(Duration::from_millis(10)) {
            Ok(()) => completed += 1,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    while let Ok(frame) = receiver.try_recv() {
        let _ = collector.write_frame(&frame);
    }
    completed
}

struct DiagnosticAccumulator {
    head: Vec<u8>,
    tail: Vec<u8>,
    omitted: bool,
}

#[cfg(test)]
fn bounded_diagnostic(bytes: &[u8]) -> String {
    let mut accumulator = DiagnosticAccumulator::new();
    accumulator.push(bytes);
    accumulator.finish()
}

impl DiagnosticAccumulator {
    fn new() -> Self {
        Self {
            head: Vec::new(),
            tail: Vec::new(),
            omitted: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        const HALF: usize = 32 * 1024;
        if self.head.len() < HALF {
            let take = (HALF - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..take]);
            if take == bytes.len() {
                return;
            }
        }
        self.omitted = true;
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > HALF {
            let extra = self.tail.len() - HALF;
            self.tail.drain(..extra);
        }
    }

    fn finish(self) -> String {
        if !self.omitted {
            return String::from_utf8_lossy(&self.head).into_owned();
        }
        let mut bounded = self.head;
        bounded.extend_from_slice(b"\n... output omitted ...\n");
        bounded.extend_from_slice(&self.tail);
        String::from_utf8_lossy(&bounded).into_owned()
    }
}

fn wait_for_exit(child: &mut Child, deadline: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => return true,
        }
    }
    false
}

#[cfg(unix)]
fn configure_process_tree(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn configure_process_tree(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
}

#[cfg(not(any(unix, windows)))]
fn configure_process_tree(_command: &mut Command) {}

#[cfg(not(windows))]
struct ProcessTree;

#[cfg(windows)]
struct ProcessTree {
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        if self.job != 0 {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.job);
            }
        }
    }
}

#[cfg(not(windows))]
fn spawn_process_tree(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    configure_process_tree(command);
    command.spawn().map(|child| (child, ProcessTree))
}

#[cfg(windows)]
fn spawn_process_tree(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
    use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
    use windows_sys::Win32::System::JobObjects::SetInformationJobObject;

    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let process_tree = ProcessTree { job };
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            process_tree.job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        return Err(std::io::Error::last_os_error());
    }

    configure_process_tree(command);
    let mut child = command.spawn()?;
    let process_handle = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    if unsafe { AssignProcessToJobObject(process_tree.job, process_handle) } == 0 {
        let error = std::io::Error::last_os_error();
        abort_suspended_process(&mut child, &process_tree);
        return Err(error);
    }

    let mut is_member = 0;
    if unsafe { IsProcessInJob(process_handle, process_tree.job, &mut is_member) } == 0 {
        let error = std::io::Error::last_os_error();
        abort_suspended_process(&mut child, &process_tree);
        return Err(error);
    }
    if is_member == 0 {
        abort_suspended_process(&mut child, &process_tree);
        return Err(std::io::Error::other(
            "spawned verifier command is not a member of its Job Object",
        ));
    }

    let resume_status = unsafe { NtResumeProcess(process_handle) };
    if resume_status < 0 {
        abort_suspended_process(&mut child, &process_tree);
        return Err(std::io::Error::other(format!(
            "NtResumeProcess failed with NTSTATUS 0x{:08x}",
            resume_status as u32
        )));
    }

    Ok((child, process_tree))
}

#[cfg(windows)]
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtResumeProcess(process_handle: windows_sys::Win32::Foundation::HANDLE) -> i32;
}

#[cfg(windows)]
fn abort_suspended_process(child: &mut Child, process_tree: &ProcessTree) {
    unsafe {
        windows_sys::Win32::System::JobObjects::TerminateJobObject(process_tree.job, 1);
    }
    let _ = child.kill();
    let _ = wait_for_exit(child, POST_TERMINATION_DRAIN);
}

#[cfg(unix)]
fn terminate_process_tree(child: &mut Child, _process_tree: &ProcessTree) {
    let pid = child.id() as i32;
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    if wait_for_exit(child, GRACEFUL_TERMINATION) {
        return;
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = wait_for_exit(child, POST_TERMINATION_DRAIN);
}

#[cfg(windows)]
fn terminate_process_tree(child: &mut Child, process_tree: &ProcessTree) {
    use windows_sys::Win32::System::Console::CTRL_BREAK_EVENT;
    use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;

    unsafe {
        GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id());
    }
    if wait_for_exit(child, GRACEFUL_TERMINATION) {
        return;
    }
    unsafe {
        TerminateJobObject(process_tree.job, 1);
    }
    let _ = wait_for_exit(child, POST_TERMINATION_DRAIN);
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_tree(child: &mut Child, _process_tree: &ProcessTree) {
    let _ = child.kill();
    let _ = wait_for_exit(child, POST_TERMINATION_DRAIN);
}

pub fn random_hex_128() -> Result<String, String> {
    secure_result::random_hex_128()
}

fn nanos_u64(nanos: u128) -> u64 {
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
