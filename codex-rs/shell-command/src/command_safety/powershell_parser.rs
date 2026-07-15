use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::LazyLock;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::runtime::Handle;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;
use tokio::sync::mpsc as tokio_mpsc;

const POWERSHELL_PARSER_SCRIPT: &str = include_str!("powershell_parser.ps1");
const PARSER_DEADLINE: Duration = Duration::from_secs(5);
const PARSER_REAP_TIMEOUT: Duration = Duration::from_secs(1);
const ACTOR_QUEUE_CAPACITY: usize = 32;
const ACTOR_POOL_CAPACITY: usize = 8;
const ACTOR_IDLE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(0);
static PARSER_RUNTIME: LazyLock<ParserRuntimeBootstrap> =
    LazyLock::new(ParserRuntimeBootstrap::new);

/// Parse through a deadline-bound actor. The synchronous API is retained for callers outside an
/// async runtime; async callers must invoke it through `spawn_blocking`.
pub(super) fn parse_with_powershell_ast(executable: &str, script: &str) -> PowershellParseOutcome {
    let started_at = Instant::now();
    let Some(deadline) = started_at.checked_add(PARSER_DEADLINE) else {
        return PowershellParseOutcome::Failed;
    };
    let Some(runtime) = PARSER_RUNTIME.get_until(deadline) else {
        return PowershellParseOutcome::Failed;
    };
    if Instant::now() >= deadline {
        return PowershellParseOutcome::Failed;
    }

    let executable = executable.to_string();
    let script = script.to_string();
    let pool = Arc::clone(&runtime.pool);
    let (response_tx, response_rx) = mpsc::channel();
    runtime.handle.spawn(async move {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            send_outcome(response_tx, PowershellParseOutcome::Failed);
            return;
        }

        let normalize_task = tokio::task::spawn_blocking(move || {
            (
                normalize_executable(executable),
                encode_powershell_base64(&script),
            )
        });
        let (identity, encoded_payload) =
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), normalize_task)
                .await
            {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(_)) | Err(_) => {
                    send_outcome(response_tx, PowershellParseOutcome::Failed);
                    return;
                }
            };
        if Instant::now() >= deadline {
            send_outcome(response_tx, PowershellParseOutcome::Failed);
            return;
        }

        route_request(pool, identity, encoded_payload, deadline, response_tx).await;
    });

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return PowershellParseOutcome::Failed;
    }
    match response_rx.recv_timeout(remaining) {
        Ok(outcome) if Instant::now() < deadline => outcome,
        Ok(_) | Err(_) => PowershellParseOutcome::Failed,
    }
}

pub(crate) fn try_parse_powershell_ast_commands(
    executable: &str,
    script: &str,
) -> Option<Vec<Vec<String>>> {
    match parse_with_powershell_ast(executable, script) {
        PowershellParseOutcome::Commands(commands) => Some(commands),
        PowershellParseOutcome::Unsupported | PowershellParseOutcome::Failed => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PowershellParseOutcome {
    Commands(Vec<Vec<String>>),
    Unsupported,
    Failed,
}

struct ParserRuntime {
    _runtime: Runtime,
    handle: Handle,
    pool: Arc<Mutex<ParserPool>>,
}

struct ParserRuntimeBootstrap {
    state: Arc<(StdMutex<RuntimeBootstrapState>, Condvar)>,
}

enum RuntimeBootstrapState {
    Starting,
    Ready(Arc<ParserRuntime>),
    Failed,
}

impl ParserRuntimeBootstrap {
    fn new() -> Self {
        let state = Arc::new((
            StdMutex::new(RuntimeBootstrapState::Starting),
            Condvar::new(),
        ));
        let thread_state = Arc::clone(&state);
        let spawn_result = std::thread::Builder::new()
            .name("codex-powershell-parser-bootstrap".to_string())
            .spawn(move || {
                let initialized = ParserRuntime::new().map(Arc::new);
                let (lock, ready) = thread_state.as_ref();
                let mut state = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *state = match initialized {
                    Ok(runtime) => RuntimeBootstrapState::Ready(runtime),
                    Err(_) => RuntimeBootstrapState::Failed,
                };
                ready.notify_all();
            });
        if spawn_result.is_err() {
            let (lock, ready) = state.as_ref();
            let mut state = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *state = RuntimeBootstrapState::Failed;
            ready.notify_all();
        }
        Self { state }
    }

    fn get_until(&self, deadline: Instant) -> Option<Arc<ParserRuntime>> {
        let (lock, ready) = self.state.as_ref();
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match &*state {
                RuntimeBootstrapState::Ready(runtime) => return Some(Arc::clone(runtime)),
                RuntimeBootstrapState::Failed => return None,
                RuntimeBootstrapState::Starting => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next_state, wait_result) = ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next_state;
            if wait_result.timed_out() && matches!(*state, RuntimeBootstrapState::Starting) {
                return None;
            }
        }
    }
}

impl ParserRuntime {
    fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("codex-powershell-parser")
            .enable_io()
            .enable_time()
            .build()?;
        let handle = runtime.handle().clone();
        Ok(Self {
            _runtime: runtime,
            handle,
            pool: Arc::new(Mutex::new(ParserPool::default())),
        })
    }
}

#[derive(Default)]
struct ParserPool {
    actors: HashMap<String, ActorHandle>,
}

#[derive(Clone)]
struct ActorHandle {
    requests: tokio_mpsc::Sender<ActorRequest>,
    state: Arc<AtomicU8>,
    generation: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum ActorState {
    Active = 0,
    Quarantined = 1,
    Stopped = 2,
}

impl ActorHandle {
    fn state(&self) -> ActorState {
        match self.state.load(Ordering::Acquire) {
            value if value == ActorState::Active as u8 => ActorState::Active,
            value if value == ActorState::Quarantined as u8 => ActorState::Quarantined,
            _ => ActorState::Stopped,
        }
    }
}

struct ActorRequest {
    generation: u64,
    request_id: u64,
    encoded_payload: String,
    deadline: Instant,
    response: mpsc::Sender<PowershellParseOutcome>,
}

#[derive(Clone)]
struct ExecutableIdentity {
    key: String,
    launch_path: PathBuf,
}

async fn route_request(
    pool: Arc<Mutex<ParserPool>>,
    identity: ExecutableIdentity,
    encoded_payload: String,
    deadline: Instant,
    response: mpsc::Sender<PowershellParseOutcome>,
) {
    if Instant::now() >= deadline {
        send_outcome(response, PowershellParseOutcome::Failed);
        return;
    }

    let mut pool = pool.lock().await;
    if Instant::now() >= deadline {
        send_outcome(response, PowershellParseOutcome::Failed);
        return;
    }
    pool.actors
        .retain(|_, handle| handle.state() != ActorState::Stopped);

    if let Some(handle) = pool.actors.get(&identity.key) {
        if handle.state() != ActorState::Active || Instant::now() >= deadline {
            send_outcome(response, PowershellParseOutcome::Failed);
            return;
        }
        let request = ActorRequest {
            generation: handle.generation.load(Ordering::Acquire),
            request_id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
            encoded_payload,
            deadline,
            response,
        };
        if let Err(error) = handle.requests.try_send(request) {
            send_outcome(error.into_inner().response, PowershellParseOutcome::Failed);
        }
        return;
    }

    if pool.actors.len() >= ACTOR_POOL_CAPACITY || Instant::now() >= deadline {
        send_outcome(response, PowershellParseOutcome::Failed);
        return;
    }

    let (requests, receiver) = tokio_mpsc::channel(ACTOR_QUEUE_CAPACITY);
    let state = Arc::new(AtomicU8::new(ActorState::Active as u8));
    let generation = Arc::new(AtomicU64::new(0));
    let handle = ActorHandle {
        requests,
        state: Arc::clone(&state),
        generation: Arc::clone(&generation),
    };
    if Instant::now() >= deadline {
        send_outcome(response, PowershellParseOutcome::Failed);
        return;
    }
    let request = ActorRequest {
        generation: 0,
        request_id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
        encoded_payload,
        deadline,
        response,
    };
    if let Err(error) = handle.requests.try_send(request) {
        send_outcome(error.into_inner().response, PowershellParseOutcome::Failed);
        return;
    }
    tokio::spawn(run_actor(identity.launch_path, receiver, state, generation));
    pool.actors.insert(identity.key, handle);
}

async fn run_actor(
    executable: PathBuf,
    requests: tokio_mpsc::Receiver<ActorRequest>,
    state: Arc<AtomicU8>,
    generation: Arc<AtomicU64>,
) {
    run_actor_with_factory(
        executable,
        requests,
        state,
        generation,
        PARSER_REAP_TIMEOUT,
        PowershellParserProcess::spawn,
    )
    .await;
}

async fn run_actor_with_factory<P, F>(
    executable: PathBuf,
    mut requests: tokio_mpsc::Receiver<ActorRequest>,
    state: Arc<AtomicU8>,
    generation: Arc<AtomicU64>,
    reap_timeout: Duration,
    mut spawn: F,
) where
    P: ParserProcess,
    F: FnMut(&PathBuf) -> std::io::Result<P> + Send,
{
    let mut parser: Option<P> = None;
    loop {
        let request = match tokio::time::timeout(ACTOR_IDLE_TTL, requests.recv()).await {
            Ok(Some(request)) => request,
            Ok(None) | Err(_) => {
                state.store(ActorState::Quarantined as u8, Ordering::Release);
                requests.close();
                while let Ok(request) = requests.try_recv() {
                    send_outcome(request.response, PowershellParseOutcome::Failed);
                }
                quarantine_and_reap(&state, &generation, &mut parser, reap_timeout).await;
                state.store(ActorState::Stopped as u8, Ordering::Release);
                return;
            }
        };

        let current_generation = generation.load(Ordering::Acquire);
        if Instant::now() >= request.deadline || request.generation != current_generation {
            send_outcome(request.response, PowershellParseOutcome::Failed);
            continue;
        }

        let outcome = process_request(
            &executable,
            &state,
            &generation,
            &mut parser,
            &request,
            reap_timeout,
            &mut spawn,
        )
        .await;
        send_outcome(request.response, outcome);
    }
}

async fn process_request<P, F>(
    executable: &PathBuf,
    state: &Arc<AtomicU8>,
    generation: &Arc<AtomicU64>,
    parser: &mut Option<P>,
    request: &ActorRequest,
    reap_timeout: Duration,
    spawn: &mut F,
) -> PowershellParseOutcome
where
    P: ParserProcess,
    F: FnMut(&PathBuf) -> std::io::Result<P> + Send,
{
    for attempt in 0..=1 {
        let remaining = request.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return PowershellParseOutcome::Failed;
        }

        if parser.is_none() {
            match spawn(executable) {
                Ok(process) => *parser = Some(process),
                Err(_) => {
                    quarantine_and_reap(state, generation, parser, reap_timeout).await;
                    if attempt == 1 || Instant::now() >= request.deadline {
                        state.store(ActorState::Active as u8, Ordering::Release);
                        return PowershellParseOutcome::Failed;
                    }
                    state.store(ActorState::Active as u8, Ordering::Release);
                    continue;
                }
            }
        }
        if Instant::now() >= request.deadline {
            quarantine_and_reap(state, generation, parser, reap_timeout).await;
            state.store(ActorState::Active as u8, Ordering::Release);
            return PowershellParseOutcome::Failed;
        }

        let parse_result = {
            let Some(parser_process) = parser.as_mut() else {
                return PowershellParseOutcome::Failed;
            };
            tokio::time::timeout_at(
                tokio::time::Instant::from_std(request.deadline),
                parser_process.parse(request.request_id, &request.encoded_payload),
            )
            .await
        };
        match parse_result {
            Ok(Ok(outcome)) if Instant::now() < request.deadline => return outcome,
            Ok(Ok(_)) => {
                quarantine_and_reap(state, generation, parser, reap_timeout).await;
                state.store(ActorState::Active as u8, Ordering::Release);
                return PowershellParseOutcome::Failed;
            }
            Ok(Err(_)) | Err(_) => {
                quarantine_and_reap(state, generation, parser, reap_timeout).await;
                if attempt == 1 || Instant::now() >= request.deadline {
                    state.store(ActorState::Active as u8, Ordering::Release);
                    return PowershellParseOutcome::Failed;
                }
                state.store(ActorState::Active as u8, Ordering::Release);
            }
        }
    }

    PowershellParseOutcome::Failed
}

async fn quarantine_and_reap<P: ParserProcess>(
    state: &Arc<AtomicU8>,
    generation: &Arc<AtomicU64>,
    parser: &mut Option<P>,
    reap_timeout: Duration,
) {
    state.store(ActorState::Quarantined as u8, Ordering::Release);
    if let Some(process) = parser.take() {
        let _ = tokio::time::timeout(reap_timeout, process.terminate_and_reap()).await;
    }
    generation.fetch_add(1, Ordering::AcqRel);
}

fn normalize_executable(executable: String) -> ExecutableIdentity {
    let resolved = which::which(&executable).unwrap_or_else(|_| PathBuf::from(&executable));
    let launch_path = std::fs::canonicalize(&resolved).unwrap_or_else(|_| {
        if resolved.is_absolute() {
            resolved
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&resolved))
                .unwrap_or(resolved)
        }
    });
    let mut key = launch_path.to_string_lossy().into_owned();
    if cfg!(windows) {
        key = key.replace('/', "\\").to_lowercase();
    }
    ExecutableIdentity { key, launch_path }
}

fn send_outcome(response: mpsc::Sender<PowershellParseOutcome>, outcome: PowershellParseOutcome) {
    let _ = response.send(outcome);
}

fn encode_powershell_base64(script: &str) -> String {
    let mut utf16 = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    BASE64_STANDARD.encode(utf16)
}

fn encoded_parser_script() -> &'static str {
    static ENCODED: LazyLock<String> =
        LazyLock::new(|| encode_powershell_base64(POWERSHELL_PARSER_SCRIPT));
    &ENCODED
}

struct PowershellParserProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

trait ParserProcess: Send {
    fn parse<'a>(
        &'a mut self,
        request_id: u64,
        encoded_payload: &'a str,
    ) -> impl Future<Output = std::io::Result<PowershellParseOutcome>> + Send + 'a;

    fn terminate_and_reap(self) -> impl Future<Output = ()> + Send;
}

impl PowershellParserProcess {
    fn spawn(executable: &PathBuf) -> std::io::Result<Self> {
        let mut command = Command::new(executable);
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                encoded_parser_script(),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::BrokenPipe,
                "PowerShell parser child did not expose stdin",
            )
        })?;
        let stdout = child.stdout.take().map(BufReader::new).ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::BrokenPipe,
                "PowerShell parser child did not expose stdout",
            )
        })?;
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    async fn parse(
        &mut self,
        request_id: u64,
        encoded_payload: &str,
    ) -> std::io::Result<PowershellParseOutcome> {
        let request_id_text = request_id.to_string();
        self.stdin.write_all(b"{\"id\":").await?;
        self.stdin.write_all(request_id_text.as_bytes()).await?;
        self.stdin.write_all(b",\"payload\":\"").await?;
        self.stdin.write_all(encoded_payload.as_bytes()).await?;
        self.stdin.write_all(b"\"}\n").await?;
        self.stdin.flush().await?;

        let mut response_line = Vec::new();
        loop {
            let available = self.stdout.fill_buf().await?;
            if available.is_empty() {
                return Err(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "PowerShell parser closed stdout",
                ));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(available.len(), |index| index.saturating_add(1));
            if response_line.len().saturating_add(take) > MAX_RESPONSE_BYTES {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "PowerShell parser response exceeded the byte limit",
                ));
            }
            response_line.extend_from_slice(&available[..take]);
            self.stdout.consume(take);
            if newline.is_some() {
                break;
            }
        }
        let response = deserialize_response(&response_line)?;
        if response.id != request_id {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "PowerShell parser returned response id {} for request {}",
                    response.id, request_id
                ),
            ));
        }

        response.into_outcome()
    }

    async fn terminate_and_reap(mut self) {
        drop(self.stdin);
        drop(self.stdout);
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

impl ParserProcess for PowershellParserProcess {
    fn parse<'a>(
        &'a mut self,
        request_id: u64,
        encoded_payload: &'a str,
    ) -> impl Future<Output = std::io::Result<PowershellParseOutcome>> + Send + 'a {
        PowershellParserProcess::parse(self, request_id, encoded_payload)
    }

    fn terminate_and_reap(self) -> impl Future<Output = ()> + Send {
        PowershellParserProcess::terminate_and_reap(self)
    }
}

fn deserialize_response(response_line: &[u8]) -> std::io::Result<PowershellParserResponse> {
    serde_json::from_slice(response_line).map_err(|error| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!("failed to parse PowerShell parser response: {error}"),
        )
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PowershellParserResponse {
    id: u64,
    status: String,
    commands: Option<Vec<Vec<String>>>,
}

impl PowershellParserResponse {
    fn into_outcome(self) -> std::io::Result<PowershellParseOutcome> {
        match self.status.as_str() {
            "ok" => self
                .commands
                .filter(|commands| {
                    !commands.is_empty()
                        && commands
                            .iter()
                            .all(|cmd| !cmd.is_empty() && cmd.iter().all(|word| !word.is_empty()))
                })
                .map(PowershellParseOutcome::Commands)
                .ok_or_else(|| {
                    std::io::Error::new(
                        ErrorKind::InvalidData,
                        "PowerShell parser returned malformed ok response",
                    )
                }),
            "unsupported" if self.commands.is_none() => Ok(PowershellParseOutcome::Unsupported),
            "parse_failed" | "parse_errors" if self.commands.is_none() => {
                Ok(PowershellParseOutcome::Failed)
            }
            _ => Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "PowerShell parser returned invalid status `{}`",
                    self.status
                ),
            )),
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::powershell::try_find_powershell_executable_blocking;
    use pretty_assertions::assert_eq;
    use std::sync::atomic::AtomicBool;

    #[derive(Clone, Copy)]
    enum FakeParseBehavior {
        Hang,
        Unsupported,
    }

    #[derive(Clone, Copy)]
    enum FakeReapBehavior {
        Hang,
        Complete,
    }

    struct FakeParserProcess {
        parse_behavior: FakeParseBehavior,
        reap_behavior: FakeReapBehavior,
        reap_attempts: Arc<AtomicU64>,
    }

    impl ParserProcess for FakeParserProcess {
        fn parse<'a>(
            &'a mut self,
            _request_id: u64,
            _encoded_payload: &'a str,
        ) -> impl Future<Output = std::io::Result<PowershellParseOutcome>> + Send + 'a {
            async move {
                match self.parse_behavior {
                    FakeParseBehavior::Hang => {
                        std::future::pending::<std::io::Result<PowershellParseOutcome>>().await
                    }
                    FakeParseBehavior::Unsupported => Ok(PowershellParseOutcome::Unsupported),
                }
            }
        }

        fn terminate_and_reap(self) -> impl Future<Output = ()> + Send {
            async move {
                self.reap_attempts.fetch_add(1, Ordering::Relaxed);
                if matches!(self.reap_behavior, FakeReapBehavior::Hang) {
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    async fn receive_outcome(
        receiver: mpsc::Receiver<PowershellParseOutcome>,
        wait: Duration,
    ) -> PowershellParseOutcome {
        tokio::task::spawn_blocking(move || receiver.recv_timeout(wait))
            .await
            .expect("response waiter should not panic")
            .expect("parser actor should respond within the test bound")
    }

    fn actor_request(
        generation: u64,
        deadline: Instant,
    ) -> (ActorRequest, mpsc::Receiver<PowershellParseOutcome>) {
        let (response, response_rx) = mpsc::channel();
        (
            ActorRequest {
                generation,
                request_id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
                encoded_payload: String::new(),
                deadline,
                response,
            },
            response_rx,
        )
    }

    fn parse(script: &str) -> Option<PowershellParseOutcome> {
        let powershell = try_find_powershell_executable_blocking()?;
        Some(parse_with_powershell_ast(
            powershell.as_path().to_str()?,
            script,
        ))
    }

    #[test]
    fn parser_actor_handles_multiple_requests() {
        let Some(first) = parse("Get-Content 'foo bar'") else {
            return;
        };
        assert_eq!(
            first,
            PowershellParseOutcome::Commands(vec![vec![
                "Get-Content".to_string(),
                "foo bar".to_string(),
            ]]),
        );

        let Some(second) = parse("Write-Output foo | Measure-Object") else {
            return;
        };
        assert_eq!(
            second,
            PowershellParseOutcome::Commands(vec![
                vec!["Write-Output".to_string(), "foo".to_string()],
                vec!["Measure-Object".to_string()],
            ]),
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hung_parser_preserves_heartbeat_and_other_executable_progress() {
        let reap_timeout = Duration::from_millis(25);
        let hung_reaps = Arc::new(AtomicU64::new(0));
        let healthy_reaps = Arc::new(AtomicU64::new(0));
        let hung_state = Arc::new(AtomicU8::new(ActorState::Active as u8));
        let healthy_state = Arc::new(AtomicU8::new(ActorState::Active as u8));
        let hung_generation = Arc::new(AtomicU64::new(0));
        let healthy_generation = Arc::new(AtomicU64::new(0));
        let (hung_tx, hung_rx) = tokio_mpsc::channel(1);
        let (healthy_tx, healthy_rx) = tokio_mpsc::channel(1);
        let hung_actor = {
            let reap_attempts = Arc::clone(&hung_reaps);
            tokio::spawn(run_actor_with_factory(
                PathBuf::from("hung-parser"),
                hung_rx,
                Arc::clone(&hung_state),
                Arc::clone(&hung_generation),
                reap_timeout,
                move |_| {
                    Ok(FakeParserProcess {
                        parse_behavior: FakeParseBehavior::Hang,
                        reap_behavior: FakeReapBehavior::Hang,
                        reap_attempts: Arc::clone(&reap_attempts),
                    })
                },
            ))
        };
        let healthy_actor = {
            let reap_attempts = Arc::clone(&healthy_reaps);
            tokio::spawn(run_actor_with_factory(
                PathBuf::from("healthy-parser"),
                healthy_rx,
                Arc::clone(&healthy_state),
                Arc::clone(&healthy_generation),
                reap_timeout,
                move |_| {
                    Ok(FakeParserProcess {
                        parse_behavior: FakeParseBehavior::Unsupported,
                        reap_behavior: FakeReapBehavior::Complete,
                        reap_attempts: Arc::clone(&reap_attempts),
                    })
                },
            ))
        };
        let (hung_request, hung_response) =
            actor_request(0, Instant::now() + Duration::from_millis(150));
        let (healthy_request, healthy_response) =
            actor_request(0, Instant::now() + Duration::from_secs(1));
        hung_tx
            .send(hung_request)
            .await
            .expect("hung actor should accept request");
        healthy_tx
            .send(healthy_request)
            .await
            .expect("healthy actor should accept request");

        let heartbeat_ticks = Arc::new(AtomicU64::new(0));
        let heartbeat_stop = Arc::new(AtomicBool::new(false));
        let heartbeat = {
            let heartbeat_ticks = Arc::clone(&heartbeat_ticks);
            let heartbeat_stop = Arc::clone(&heartbeat_stop);
            tokio::spawn(async move {
                while !heartbeat_stop.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    heartbeat_ticks.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        let hung_wait = tokio::spawn(receive_outcome(hung_response, Duration::from_secs(1)));
        let healthy_wait = tokio::spawn(receive_outcome(healthy_response, Duration::from_secs(1)));

        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(heartbeat_ticks.load(Ordering::Relaxed) >= 2);
        assert!(
            !hung_wait.is_finished(),
            "hung parser must remain isolated rather than fail the actor immediately"
        );

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), healthy_wait)
                .await
                .expect("healthy executable should progress while its peer is hung")
                .expect("healthy response task should not panic"),
            PowershellParseOutcome::Unsupported
        );
        assert!(
            !hung_wait.is_finished(),
            "healthy executable should complete before the hung request deadline"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), hung_wait)
                .await
                .expect("hung request should be deadline bounded")
                .expect("hung response task should not panic"),
            PowershellParseOutcome::Failed
        );
        assert_eq!(hung_reaps.load(Ordering::Relaxed), 1);
        assert_eq!(hung_generation.load(Ordering::Acquire), 1);

        heartbeat_stop.store(true, Ordering::Release);
        heartbeat.await.expect("heartbeat task should stop");
        drop(hung_tx);
        drop(healthy_tx);
        tokio::time::timeout(Duration::from_secs(1), hung_actor)
            .await
            .expect("hung actor should stop after its queue closes")
            .expect("hung actor task should not panic");
        tokio::time::timeout(Duration::from_secs(1), healthy_actor)
            .await
            .expect("healthy actor should stop after its queue closes")
            .expect("healthy actor task should not panic");
        assert_eq!(
            hung_state.load(Ordering::Acquire),
            ActorState::Stopped as u8
        );
        assert_eq!(
            healthy_state.load(Ordering::Acquire),
            ActorState::Stopped as u8
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timed_out_parser_restarts_for_a_later_request() {
        let reap_timeout = Duration::from_millis(25);
        let spawn_count = Arc::new(AtomicU64::new(0));
        let reap_attempts = Arc::new(AtomicU64::new(0));
        let state = Arc::new(AtomicU8::new(ActorState::Active as u8));
        let generation = Arc::new(AtomicU64::new(0));
        let (request_tx, request_rx) = tokio_mpsc::channel(1);
        let actor = {
            let spawn_count = Arc::clone(&spawn_count);
            let reap_attempts = Arc::clone(&reap_attempts);
            tokio::spawn(run_actor_with_factory(
                PathBuf::from("restartable-parser"),
                request_rx,
                Arc::clone(&state),
                Arc::clone(&generation),
                reap_timeout,
                move |_| {
                    let launch = spawn_count.fetch_add(1, Ordering::Relaxed);
                    Ok(FakeParserProcess {
                        parse_behavior: if launch == 0 {
                            FakeParseBehavior::Hang
                        } else {
                            FakeParseBehavior::Unsupported
                        },
                        reap_behavior: if launch == 0 {
                            FakeReapBehavior::Hang
                        } else {
                            FakeReapBehavior::Complete
                        },
                        reap_attempts: Arc::clone(&reap_attempts),
                    })
                },
            ))
        };
        let (first_request, first_response) =
            actor_request(0, Instant::now() + Duration::from_millis(100));
        request_tx
            .send(first_request)
            .await
            .expect("actor should accept the first request");
        assert_eq!(
            receive_outcome(first_response, Duration::from_secs(1)).await,
            PowershellParseOutcome::Failed
        );
        assert_eq!(state.load(Ordering::Acquire), ActorState::Active as u8);
        assert_eq!(generation.load(Ordering::Acquire), 1);
        assert_eq!(spawn_count.load(Ordering::Relaxed), 1);
        assert_eq!(reap_attempts.load(Ordering::Relaxed), 1);

        let (second_request, second_response) =
            actor_request(1, Instant::now() + Duration::from_secs(1));
        request_tx
            .send(second_request)
            .await
            .expect("actor should accept a later request");
        assert_eq!(
            receive_outcome(second_response, Duration::from_secs(1)).await,
            PowershellParseOutcome::Unsupported
        );
        assert_eq!(spawn_count.load(Ordering::Relaxed), 2);

        drop(request_tx);
        tokio::time::timeout(Duration::from_secs(1), actor)
            .await
            .expect("restartable actor should stop after its queue closes")
            .expect("restartable actor task should not panic");
        assert_eq!(state.load(Ordering::Acquire), ActorState::Stopped as u8);
        assert_eq!(reap_attempts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn parser_process_rejects_stop_parsing_forms() {
        let Some(parsed) = parse("git log --% HEAD --output=codex_poc.txt") else {
            return;
        };
        assert_eq!(parsed, PowershellParseOutcome::Unsupported);
    }

    #[test]
    fn parser_process_rejects_param_blocks() {
        let Some(parsed) = parse("param([string]$path = (Get-Location)) Write-Output test") else {
            return;
        };
        assert_eq!(parsed, PowershellParseOutcome::Unsupported);
    }

    #[test]
    fn parser_process_rejects_named_blocks() {
        let Some(parsed) =
            parse("begin { Set-Content codex_poc.txt pwned } end { Get-Content Cargo.toml }")
        else {
            return;
        };
        assert_eq!(parsed, PowershellParseOutcome::Unsupported);
    }

    #[test]
    fn parser_process_rejects_using_statements() {
        let Some(parsed) = parse("using module ./codex_poc.psm1\nGet-Content Cargo.toml") else {
            return;
        };
        assert_eq!(parsed, PowershellParseOutcome::Unsupported);
    }

    #[test]
    fn parser_process_rejects_trap_blocks() {
        let Some(parsed) = parse(
            "trap { Set-Content codex_poc.txt pwned; continue } Get-Content missing -ErrorAction Stop",
        ) else {
            return;
        };
        assert_eq!(parsed, PowershellParseOutcome::Unsupported);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_queued_request_does_not_start_a_parser() {
        let (request_tx, request_rx) = tokio_mpsc::channel(1);
        let state = Arc::new(AtomicU8::new(ActorState::Active as u8));
        let generation = Arc::new(AtomicU64::new(4));
        let (response_tx, response_rx) = mpsc::channel();
        request_tx
            .send(ActorRequest {
                generation: 4,
                request_id: 9,
                encoded_payload: encode_powershell_base64("Get-Content Cargo.toml"),
                deadline: Instant::now() - Duration::from_millis(1),
                response: response_tx,
            })
            .await
            .expect("actor queue should accept test request");
        drop(request_tx);

        run_actor(
            PathBuf::from("definitely-not-a-powershell-executable"),
            request_rx,
            Arc::clone(&state),
            generation,
        )
        .await;

        assert_eq!(
            response_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("expired request should be rejected"),
            PowershellParseOutcome::Failed
        );
        assert_eq!(state.load(Ordering::Acquire), ActorState::Stopped as u8);
    }

    #[test]
    fn parser_response_shapes_are_strict() {
        let ok = deserialize_response(br#"{"id":1,"status":"ok","commands":[["git","status"]]}"#)
            .expect("valid response")
            .into_outcome()
            .expect("valid outcome");
        assert_eq!(
            ok,
            PowershellParseOutcome::Commands(vec![vec!["git".to_string(), "status".to_string(),]])
        );
        for malformed in [
            br#"{"id":1,"status":"ok","commands":null}"#.as_slice(),
            br#"{"id":1,"status":"ok","commands":[]}"#.as_slice(),
            br#"{"id":1,"status":"unsupported","commands":[["git"]]}"#.as_slice(),
            br#"{"id":1,"status":"unexpected","commands":null}"#.as_slice(),
        ] {
            assert!(
                deserialize_response(malformed)
                    .expect("shape is valid JSON")
                    .into_outcome()
                    .is_err()
            );
        }
        assert!(
            deserialize_response(br#"{"id":1,"status":"ok","commands":[["git"]],"extra":1}"#)
                .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn route_request_fails_closed_when_actor_queue_is_saturated() {
        let pool = Arc::new(Mutex::new(ParserPool::default()));
        let (requests, receiver) = tokio_mpsc::channel(ACTOR_QUEUE_CAPACITY);
        let state = Arc::new(AtomicU8::new(ActorState::Active as u8));
        let generation = Arc::new(AtomicU64::new(0));
        for request_id in 0..ACTOR_QUEUE_CAPACITY as u64 {
            let (response, _response_rx) = mpsc::channel();
            requests
                .try_send(ActorRequest {
                    generation: 0,
                    request_id,
                    encoded_payload: String::new(),
                    deadline: Instant::now() + Duration::from_secs(1),
                    response,
                })
                .expect("fill actor queue");
        }
        pool.lock().await.actors.insert(
            "saturated".to_string(),
            ActorHandle {
                requests,
                state,
                generation,
            },
        );
        let (response, response_rx) = mpsc::channel();
        let started = Instant::now();

        route_request(
            Arc::clone(&pool),
            ExecutableIdentity {
                key: "saturated".to_string(),
                launch_path: PathBuf::from("unused"),
            },
            String::new(),
            Instant::now() + Duration::from_secs(1),
            response,
        )
        .await;

        assert_eq!(
            response_rx
                .recv_timeout(Duration::from_millis(50))
                .expect("saturated route should respond"),
            PowershellParseOutcome::Failed
        );
        assert!(started.elapsed() < Duration::from_millis(50));
        drop(receiver);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn route_request_rejects_ninth_executable_identity() {
        let pool = Arc::new(Mutex::new(ParserPool::default()));
        let mut receivers = Vec::new();
        {
            let mut guard = pool.lock().await;
            for index in 0..ACTOR_POOL_CAPACITY {
                let (requests, receiver) = tokio_mpsc::channel(ACTOR_QUEUE_CAPACITY);
                receivers.push(receiver);
                guard.actors.insert(
                    format!("actor-{index}"),
                    ActorHandle {
                        requests,
                        state: Arc::new(AtomicU8::new(ActorState::Active as u8)),
                        generation: Arc::new(AtomicU64::new(0)),
                    },
                );
            }
        }
        let (response, response_rx) = mpsc::channel();

        route_request(
            Arc::clone(&pool),
            ExecutableIdentity {
                key: "actor-over-cap".to_string(),
                launch_path: PathBuf::from("unused"),
            },
            String::new(),
            Instant::now() + Duration::from_secs(1),
            response,
        )
        .await;

        assert_eq!(
            response_rx
                .recv_timeout(Duration::from_millis(50))
                .expect("pool cap should respond"),
            PowershellParseOutcome::Failed
        );
        assert_eq!(pool.lock().await.actors.len(), ACTOR_POOL_CAPACITY);
        drop(receivers);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_route_request_does_not_create_an_actor() {
        let pool = Arc::new(Mutex::new(ParserPool::default()));
        let (response, response_rx) = mpsc::channel();

        route_request(
            Arc::clone(&pool),
            ExecutableIdentity {
                key: "expired".to_string(),
                launch_path: PathBuf::from("unused"),
            },
            String::new(),
            Instant::now() - Duration::from_millis(1),
            response,
        )
        .await;

        assert_eq!(
            response_rx
                .recv_timeout(Duration::from_millis(50))
                .expect("expired route should respond"),
            PowershellParseOutcome::Failed
        );
        assert!(pool.lock().await.actors.is_empty());
    }
}
