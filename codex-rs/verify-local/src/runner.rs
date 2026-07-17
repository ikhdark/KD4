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
use serde::Deserialize;
use serde::Serialize;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStderr;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

const MAX_FRAME_PAYLOAD: usize = 16 * 1024;
const COLLECTOR_CHANNEL_FRAMES: usize = 64;
const GRACEFUL_TERMINATION: Duration = Duration::from_secs(5);
const POST_TERMINATION_DRAIN: Duration = Duration::from_secs(2);
const READER_STOP_MARGIN: Duration = Duration::from_millis(100);
const READER_POLL_INTERVAL: Duration = Duration::from_millis(2);
static NEVER_CANCEL: AtomicBool = AtomicBool::new(false);

pub fn execute_plan(plan: &PlanEnvelopeV2, repository_root: &Path) -> Vec<CommandResultV2> {
    execute_plan_with_cancellation(plan, repository_root, &NEVER_CANCEL)
}

pub fn execute_plan_with_cancellation(
    plan: &PlanEnvelopeV2,
    repository_root: &Path,
    cancelled: &AtomicBool,
) -> Vec<CommandResultV2> {
    let invocation_nonce = match secure_result::random_hex_128() {
        Ok(nonce) => nonce,
        Err(error) => {
            return setup_failure_results(
                plan,
                format!("operating-system cryptographic RNG failed: {error}"),
            );
        }
    };
    let result_dir = match secure_result::create_invocation_dir(
        repository_root,
        &plan.invocation_id,
        &invocation_nonce,
    ) {
        Ok(result_dir) => result_dir,
        Err(error) => {
            return setup_failure_results(
                plan,
                format!("failed to allocate private result directory: {error}"),
            );
        }
    };
    plan.commands
        .iter()
        .enumerate()
        .map(|(ordinal, command)| {
            execute_command(
                plan,
                command,
                ordinal,
                repository_root,
                &result_dir,
                cancelled,
            )
        })
        .collect()
}

fn execute_command(
    plan: &PlanEnvelopeV2,
    command: &CommandSpecV2,
    ordinal: usize,
    repository_root: &Path,
    result_dir: &Path,
    cancellation: &AtomicBool,
) -> CommandResultV2 {
    let nonce = match secure_result::random_hex_128() {
        Ok(nonce) => nonce,
        Err(error) => {
            return setup_failure_result(
                plan,
                command,
                ordinal,
                format!("operating-system cryptographic RNG failed: {error}"),
            );
        }
    };
    let mut result = base_result(plan, command, ordinal, nonce);
    if cancellation.load(Ordering::Acquire) {
        result.cancelled = true;
        return persist_result(result_dir, result);
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

    let collector_started = Instant::now();
    let (frame_tx, frame_rx) = mpsc::sync_channel(COLLECTOR_CHANNEL_FRAMES);
    let (done_tx, done_rx) = mpsc::channel();
    let mut readers = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_reader(
            "stdout",
            PipeReader::Stdout(stdout),
            frame_tx.clone(),
            done_tx.clone(),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_reader(
            "stderr",
            PipeReader::Stderr(stderr),
            frame_tx.clone(),
            done_tx.clone(),
        ));
    }
    drop(frame_tx);
    drop(done_tx);

    let expected_readers = readers.len();
    let mut collector = FrameCollector::new(log.file, collector_started);
    let timeout = Duration::from_millis(command.timeout_ms.max(1));
    let mut exit_status = None;
    let mut collector_fault = None;
    let mut completed_readers = 0;
    loop {
        collect_reader_completions(&done_rx, &mut completed_readers, &mut collector_fault);
        while let Ok(frame) = frame_rx.try_recv() {
            if let Err(error) = collector.write_frame(&frame) {
                collector_fault = Some(error);
                terminate_process_tree(&mut child, &process_tree);
                break;
            }
        }
        if collector_fault.is_some() {
            stop_readers(&readers);
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
            Ok(None) if cancellation.load(Ordering::Acquire) => {
                result.cancelled = true;
                terminate_process_tree(&mut child, &process_tree);
                result.log_state = LogState::IncompleteAfterTermination;
                break;
            }
            Ok(None) => match frame_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(frame) => {
                    if let Err(error) = collector.write_frame(&frame) {
                        collector_fault = Some(error);
                        terminate_process_tree(&mut child, &process_tree);
                        stop_readers(&readers);
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

    let drain = drain_after_exit(
        &frame_rx,
        &done_rx,
        &mut collector,
        &mut readers,
        completed_readers,
        collector_fault.is_some(),
    );
    completed_readers = drain.completed_readers;
    if collector_fault.is_none() {
        collector_fault = drain.fault;
    }
    if (completed_readers < expected_readers || drain.forced_stop)
        && collector_fault.is_none()
        && !result.timed_out
        && !result.cancelled
    {
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
    if let Some(error) = collector_fault {
        result.runner_error = Some(error.message);
        result.log_state = error.state;
    }
    if let Err(error) = collector.finish() {
        result.runner_error = Some(error.message);
        result.log_state = error.state;
    } else if let Err(error) = verify_framed_log(&log.path, collector.seq) {
        result.runner_error = Some(error);
        result.log_state = LogState::IntegrityFailure;
    }
    result.diagnostic = collector.diagnostic.finish();
    persist_result(result_dir, result)
}

fn persist_result(result_dir: &Path, mut result: CommandResultV2) -> CommandResultV2 {
    match secure_result::write_result_file(result_dir, &result) {
        Ok(parsed) => parsed,
        Err(error) => {
            result.log_state = LogState::IntegrityFailure;
            result.runner_error = Some(error);
            result
        }
    }
}

fn setup_failure_results(plan: &PlanEnvelopeV2, error: String) -> Vec<CommandResultV2> {
    plan.commands
        .iter()
        .enumerate()
        .map(|(ordinal, command)| setup_failure_result(plan, command, ordinal, error.clone()))
        .collect()
}

fn setup_failure_result(
    plan: &PlanEnvelopeV2,
    command: &CommandSpecV2,
    ordinal: usize,
    error: String,
) -> CommandResultV2 {
    let mut result = base_result(plan, command, ordinal, String::new());
    result.runner_error = Some(error);
    result.log_state = LogState::IntegrityFailure;
    result
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
        exact_output_artifact: None,
        diagnostic_omission: None,
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

#[derive(Deserialize, Serialize)]
struct LogFrame<'a> {
    seq: u64,
    stream: &'a str,
    monotonic_ns: u64,
    bytes_base64: String,
}

#[derive(Deserialize, Serialize)]
struct OwnedLogFrame {
    seq: u64,
    stream: String,
    monotonic_ns: u64,
    bytes_base64: String,
}

struct StreamFrame {
    stream: &'static str,
    bytes: Vec<u8>,
}

struct FrameCollector {
    file: File,
    seq: u64,
    started: Instant,
    diagnostic: DiagnosticAccumulator,
}

impl FrameCollector {
    fn new(file: File, started: Instant) -> Self {
        Self {
            file,
            seq: 0,
            started,
            diagnostic: DiagnosticAccumulator::new(),
        }
    }

    fn write_frame(&mut self, frame: &StreamFrame) -> Result<(), CollectorFault> {
        let log_frame = LogFrame {
            seq: self.seq,
            stream: frame.stream,
            monotonic_ns: nanos_u64(self.started.elapsed().as_nanos()),
            bytes_base64: BASE64_STANDARD.encode(&frame.bytes),
        };
        let mut line = serde_json::to_vec(&log_frame).map_err(|error| CollectorFault {
            state: LogState::FramingFailure,
            message: error.to_string(),
        })?;
        line.push(b'\n');
        self.file.write_all(&line).map_err(CollectorFault::io)?;
        self.diagnostic.push(&line);
        self.seq += 1;
        Ok(())
    }

    fn finish(&mut self) -> Result<(), CollectorFault> {
        self.file.flush().map_err(CollectorFault::io)?;
        self.file.sync_all().map_err(CollectorFault::io)
    }
}

#[derive(Debug)]
struct CollectorFault {
    state: LogState,
    message: String,
}

impl CollectorFault {
    fn io(error: std::io::Error) -> Self {
        Self {
            state: LogState::IoFailure,
            message: error.to_string(),
        }
    }
}

enum PipeReader {
    Stdout(ChildStdout),
    Stderr(ChildStderr),
}

impl Read for PipeReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Stdout(reader) => reader.read(buffer),
            Self::Stderr(reader) => reader.read(buffer),
        }
    }
}

struct ReaderThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

struct ReaderCompletion {
    stream: &'static str,
    error: Option<String>,
}

enum PipeRead {
    Data(usize),
    Pending,
    Eof,
}

fn spawn_reader(
    stream: &'static str,
    mut reader: PipeReader,
    sender: mpsc::SyncSender<StreamFrame>,
    done: mpsc::Sender<ReaderCompletion>,
) -> ReaderThread {
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let mut completion = ReaderCompletion {
            stream,
            error: None,
        };
        if let Err(error) = configure_pipe_reader(&reader) {
            completion.error = Some(error.to_string());
            let _ = done.send(completion);
            return;
        }
        let mut buffer = vec![0_u8; MAX_FRAME_PAYLOAD];
        loop {
            if reader_stop.load(Ordering::Acquire) {
                break;
            }
            match read_pipe(&mut reader, &mut buffer) {
                Ok(PipeRead::Eof) => break,
                Ok(PipeRead::Pending) => thread::sleep(READER_POLL_INTERVAL),
                Ok(PipeRead::Data(count)) => {
                    let frame = StreamFrame {
                        stream,
                        bytes: buffer[..count].to_vec(),
                    };
                    if !send_with_backpressure(&sender, &reader_stop, frame) {
                        break;
                    }
                }
                Err(error) => {
                    completion.error = Some(error.to_string());
                    break;
                }
            }
        }
        drop(reader);
        let _ = done.send(completion);
    });
    ReaderThread {
        stop,
        handle: Some(handle),
    }
}

fn send_with_backpressure(
    sender: &mpsc::SyncSender<StreamFrame>,
    stop: &AtomicBool,
    mut frame: StreamFrame,
) -> bool {
    loop {
        match sender.try_send(frame) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Full(returned)) => {
                frame = returned;
                if stop.load(Ordering::Acquire) {
                    return false;
                }
                thread::sleep(READER_POLL_INTERVAL);
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
}

struct DrainOutcome {
    completed_readers: usize,
    forced_stop: bool,
    fault: Option<CollectorFault>,
}

fn drain_after_exit(
    receiver: &mpsc::Receiver<StreamFrame>,
    done: &mpsc::Receiver<ReaderCompletion>,
    collector: &mut FrameCollector,
    readers: &mut [ReaderThread],
    mut completed_readers: usize,
    stop_immediately: bool,
) -> DrainOutcome {
    let deadline = Instant::now() + POST_TERMINATION_DRAIN;
    let stop_at = deadline.checked_sub(READER_STOP_MARGIN).unwrap_or(deadline);
    let mut stop_requested = stop_immediately;
    let mut forced_stop = false;
    let mut fault = None;
    if stop_requested {
        stop_readers(readers);
    }
    while Instant::now() < deadline {
        if !stop_requested && Instant::now() >= stop_at {
            stop_requested = true;
            forced_stop = true;
            stop_readers(readers);
        }
        while let Ok(frame) = receiver.try_recv() {
            if let Err(error) = collector.write_frame(&frame) {
                fault = Some(error);
                stop_requested = true;
                stop_readers(readers);
                break;
            }
        }
        collect_reader_completions(done, &mut completed_readers, &mut fault);
        join_finished_readers(readers, &mut fault);
        if fault.is_some() && !stop_requested {
            stop_requested = true;
            stop_readers(readers);
        }
        if completed_readers >= readers.len()
            && readers.iter().all(|reader| reader.handle.is_none())
        {
            break;
        }
        thread::sleep(READER_POLL_INTERVAL);
    }
    if completed_readers < readers.len() {
        forced_stop = true;
        stop_readers(readers);
    }
    while let Ok(frame) = receiver.try_recv() {
        if let Err(error) = collector.write_frame(&frame) {
            fault.get_or_insert(error);
            break;
        }
    }
    collect_reader_completions(done, &mut completed_readers, &mut fault);
    join_finished_readers(readers, &mut fault);
    DrainOutcome {
        completed_readers,
        forced_stop,
        fault,
    }
}

fn stop_readers(readers: &[ReaderThread]) {
    for reader in readers {
        reader.stop.store(true, Ordering::Release);
    }
}

fn collect_reader_completions(
    done: &mpsc::Receiver<ReaderCompletion>,
    completed: &mut usize,
    fault: &mut Option<CollectorFault>,
) {
    while let Ok(completion) = done.try_recv() {
        *completed += 1;
        if let Some(error) = completion.error {
            fault.get_or_insert(CollectorFault {
                state: LogState::IoFailure,
                message: format!("{} reader failed: {error}", completion.stream),
            });
        }
    }
}

fn join_finished_readers(readers: &mut [ReaderThread], fault: &mut Option<CollectorFault>) {
    for reader in readers {
        let finished = reader.handle.as_ref().is_some_and(JoinHandle::is_finished);
        if finished
            && let Some(handle) = reader.handle.take()
            && handle.join().is_err()
        {
            fault.get_or_insert(CollectorFault {
                state: LogState::IoFailure,
                message: "pipe reader thread panicked".to_string(),
            });
        }
    }
}

#[cfg(unix)]
fn configure_pipe_reader(reader: &PipeReader) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let descriptor = match reader {
        PipeReader::Stdout(reader) => reader.as_raw_fd(),
        PipeReader::Stderr(reader) => reader.as_raw_fd(),
    };
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn configure_pipe_reader(_reader: &PipeReader) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn configure_pipe_reader(_reader: &PipeReader) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn read_pipe(reader: &mut PipeReader, buffer: &mut [u8]) -> std::io::Result<PipeRead> {
    loop {
        match reader.read(buffer) {
            Ok(0) => return Ok(PipeRead::Eof),
            Ok(count) => return Ok(PipeRead::Data(count)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(PipeRead::Pending);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn read_pipe(reader: &mut PipeReader, buffer: &mut [u8]) -> std::io::Result<PipeRead> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
    use windows_sys::Win32::Foundation::ERROR_NO_DATA;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let handle = match reader {
        PipeReader::Stdout(reader) => reader.as_raw_handle(),
        PipeReader::Stderr(reader) => reader.as_raw_handle(),
    } as windows_sys::Win32::Foundation::HANDLE;
    let mut available = 0_u32;
    if unsafe {
        PeekNamedPipe(
            handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(code) if code == ERROR_BROKEN_PIPE as i32 || code == ERROR_NO_DATA as i32
        ) {
            return Ok(PipeRead::Eof);
        }
        return Err(error);
    }
    if available == 0 {
        return Ok(PipeRead::Pending);
    }
    let read_limit = buffer.len().min(available as usize);
    let count = reader.read(&mut buffer[..read_limit])?;
    if count == 0 {
        Ok(PipeRead::Eof)
    } else {
        Ok(PipeRead::Data(count))
    }
}

#[cfg(not(any(unix, windows)))]
fn read_pipe(reader: &mut PipeReader, buffer: &mut [u8]) -> std::io::Result<PipeRead> {
    match reader.read(buffer)? {
        0 => Ok(PipeRead::Eof),
        count => Ok(PipeRead::Data(count)),
    }
}

fn verify_framed_log(path: &Path, expected_frames: u64) -> Result<(), String> {
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|error| format!("reopen framed log: {error}"))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut expected_seq = 0_u64;
    let mut previous_monotonic_ns = 0_u64;
    loop {
        line.clear();
        let count = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("read framed log: {error}"))?;
        if count == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            return Err("framed log has an unterminated final frame".to_string());
        }
        let body = &line[..line.len() - 1];
        let frame: OwnedLogFrame = secure_result::parse_exact_json(body)
            .map_err(|error| format!("parse framed log: {error}"))?;
        let canonical = serde_json::to_vec(&frame).map_err(|error| error.to_string())?;
        if canonical != body {
            return Err("framed log contains non-canonical frame bytes".to_string());
        }
        if frame.seq != expected_seq {
            return Err("framed log sequence is not contiguous".to_string());
        }
        if frame.stream != "stdout" && frame.stream != "stderr" {
            return Err("framed log contains an unknown stream".to_string());
        }
        if frame.monotonic_ns < previous_monotonic_ns {
            return Err("framed log timestamps are not monotonic".to_string());
        }
        let payload = BASE64_STANDARD
            .decode(frame.bytes_base64.as_bytes())
            .map_err(|error| format!("decode framed log payload: {error}"))?;
        if payload.len() > MAX_FRAME_PAYLOAD {
            return Err("framed log payload exceeds the frame limit".to_string());
        }
        previous_monotonic_ns = frame.monotonic_ns;
        expected_seq += 1;
    }
    if expected_seq != expected_frames {
        return Err("framed log frame count changed after flush".to_string());
    }
    Ok(())
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
