use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs;
#[cfg(windows)]
use std::fs::File as ChildStdin;
#[cfg(windows)]
use std::fs::File as ChildStdout;
use std::io::BufRead;
use std::io::BufReader;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(windows))]
use std::process::Child;
#[cfg(not(windows))]
use std::process::ChildStdin;
#[cfg(not(windows))]
use std::process::ChildStdout;
#[cfg(not(windows))]
use std::process::Command;
#[cfg(not(windows))]
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::TryLockError;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;
#[cfg(windows)]
use windows_process::Child;

const POWERSHELL_PARSER_SCRIPT: &str = include_str!("powershell_parser.ps1");
#[cfg(windows)]
mod windows_process;
const POWERSHELL_PARSER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CACHED_SYNTAX_BYTES: usize = 64 * 1024;
const MAX_CACHED_SYNTAX_ENTRIES: usize = 32;

type ScopedSyntaxEntries = HashMap<(String, String), Arc<ScopedSyntax>>;

/// Invocation-local syntax evidence. Never use this to cache executable
/// resolution: cwd, PATH, PATHEXT and filesystem state remain live inputs.
#[derive(Clone, Default)]
pub struct PowershellAnalysisScope {
    entries: Arc<Mutex<ScopedSyntaxEntries>>,
}

struct ScopedSyntax {
    deadline: Instant,
    outcome: std::sync::OnceLock<PowershellParseOutcome>,
    unavailable: std::sync::atomic::AtomicBool,
}

tokio::task_local! {
    static ACTIVE_ANALYSIS: PowershellAnalysisScope;
}
thread_local! {
    static BLOCKING_ANALYSIS: std::cell::RefCell<Option<PowershellAnalysisScope>> = const { std::cell::RefCell::new(None) };
}

impl PowershellAnalysisScope {
    pub fn current() -> Option<Self> {
        ACTIVE_ANALYSIS.try_with(Clone::clone).ok()
            .or_else(|| BLOCKING_ANALYSIS.with(|slot| slot.borrow().clone()))
    }

    pub async fn scope<F: std::future::Future>(self, future: F) -> F::Output {
        ACTIVE_ANALYSIS.scope(self, future).await
    }

    pub fn scope_sync<T>(context: Option<Self>, operation: impl FnOnce() -> T) -> T {
        struct Restore(Option<PowershellAnalysisScope>);
        impl Drop for Restore {
            fn drop(&mut self) {
                BLOCKING_ANALYSIS.with(|slot| slot.replace(self.0.take()));
            }
        }
        let _restore = Restore(BLOCKING_ANALYSIS.with(|slot| slot.replace(context)));
        operation()
    }

    fn syntax_for(
        &self, executable: &str, script: &str,
        parse: impl FnOnce(Instant) -> PowershellParseOutcome,
    ) -> Option<(Arc<ScopedSyntax>, PowershellParseOutcome)> {
        let key = (executable.to_owned(), script.to_owned());
        let entry = {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            if !entries.contains_key(&key) && entries.len() >= MAX_CACHED_SYNTAX_ENTRIES {
                return None; // Fail closed instead of evicting a failure and retrying it.
            }
            Arc::clone(entries.entry(key).or_insert_with(|| Arc::new(ScopedSyntax {
                deadline: Instant::now() + POWERSHELL_PARSER_RESPONSE_TIMEOUT,
                outcome: std::sync::OnceLock::new(),
                unavailable: std::sync::atomic::AtomicBool::new(false),
            })))
        };
        // No map lock is held while the one parser owner runs. Other workers
        // wait for that bounded request rather than starting spare hosts.
        let outcome = entry.outcome.get_or_init(|| parse(entry.deadline)).clone();
        Some((entry, outcome))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum PowershellFlavor {
    WindowsPowerShell,
    Pwsh,
}

type CachedParser = Arc<Mutex<Option<PowershellParserProcess>>>;

type SpareParsers = HashMap<PowershellFlavor, Option<PowershellParserProcess>>;
const MAX_SPARE_PARSERS: usize = 4;
static SPARE_PARSERS: LazyLock<[Mutex<SpareParsers>; MAX_SPARE_PARSERS]> =
    LazyLock::new(|| std::array::from_fn(|_| Mutex::new(HashMap::new())));

static PARSER_PROCESSES: LazyLock<Mutex<HashMap<PowershellFlavor, CachedParser>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PARSER_AVAILABILITY: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

fn notify_parser_available() {
    let _guard = PARSER_AVAILABILITY
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    PARSER_AVAILABILITY.1.notify_all();
}

fn wait_for_cached_parser<'a>(
    parser: &'a CachedParser,
    spare_parsers: &'a [Mutex<SpareParsers>],
    deadline: Instant,
) -> Option<CachedParserAccess<'a>> {
    let mut available = PARSER_AVAILABILITY
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        match acquire_cached_parser(parser, spare_parsers) {
            CachedParserAccess::Saturated => {}
            parser => return Some(parser),
        }
        available = PARSER_AVAILABILITY
            .1
            .wait_timeout(available, remaining)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

/// Warm the same trusted host used by command analysis. Speculation never
/// waits for an occupied parser or consumes the spare foreground parser.
pub fn prewarm_powershell_parser(executable: &str) {
    let Some(flavor) = PowershellFlavor::from_requested_executable(executable) else {
        return;
    };
    let parser = PARSER_PROCESSES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(flavor)
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone();
    if let Ok(mut parser) = parser.try_lock()
        && parser.is_none()
    {
        let _ = parse_with_cached_process(
            &mut parser,
            executable,
            "",
            None,
            Instant::now() + POWERSHELL_PARSER_RESPONSE_TIMEOUT,
        );
    }
    notify_parser_available();
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PowershellResolutionState {
    pub cwd: String,
    pub path: String,
    pub pathext: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PowershellInvocation<'a> {
    InlineCommand { script: &'a str, no_profile: bool },
    Opaque,
    Bare,
    Empty,
    Invalid,
}

/// Parse the PowerShell host argument envelope once for every command-safety consumer.
///
/// Only `-Command` exposes source that can be inspected. File, encoded, bare, and unknown forms
/// remain opaque so callers cannot accidentally reinterpret them as inline script text.
pub(crate) fn parse_powershell_invocation(args: &[String]) -> PowershellInvocation<'_> {
    if args.is_empty() {
        return PowershellInvocation::Empty;
    }

    let mut idx = 0;
    let mut no_profile = false;
    while idx < args.len() {
        let arg = &args[idx];
        let lower = arg.to_ascii_lowercase();
        match lower.as_str() {
            "-command" | "/command" | "-c" => {
                let Some(script) = args.get(idx + 1) else {
                    return PowershellInvocation::Invalid;
                };
                if idx + 2 != args.len() {
                    return PowershellInvocation::Invalid;
                }
                // A lone dash reads source from stdin; it is not inline script text.
                if script == "-" {
                    return PowershellInvocation::Opaque;
                }
                return PowershellInvocation::InlineCommand { script, no_profile };
            }
            _ if lower.starts_with("-command:") || lower.starts_with("/command:") => {
                if idx + 1 != args.len() {
                    return PowershellInvocation::Invalid;
                }
                let Some((_, script)) = arg.split_once(':') else {
                    return PowershellInvocation::Invalid;
                };
                if script == "-" {
                    return PowershellInvocation::Opaque;
                }
                return PowershellInvocation::InlineCommand { script, no_profile };
            }
            "-noprofile" => {
                no_profile = true;
                idx += 1;
            }
            "-nologo" | "-noninteractive" | "-mta" | "-sta" => {
                idx += 1;
            }
            "-encodedcommand" | "-ec" | "-file" | "/file" | "-windowstyle" | "-executionpolicy"
            | "-workingdirectory" => {
                return PowershellInvocation::Opaque;
            }
            _ if lower.starts_with('-') => return PowershellInvocation::Opaque,
            _ => return PowershellInvocation::Bare,
        }
    }

    PowershellInvocation::Empty
}

/// Cache one long-lived parser process per trusted PowerShell flavor. The map lock only protects
/// cache lookup; each parser has its own lock so a stalled host cannot block the other flavor.
pub(super) fn parse_with_powershell_ast(executable: &str, script: &str) -> PowershellParseOutcome {
    parse_with_powershell_ast_request(executable, script, None)
}

/// Only a positive syntax-error report is a rejection. An unavailable parser
/// or an unsupported (but possibly valid) AST remains the target shell's job.
pub fn powershell_command_has_syntax_error(command: &[String]) -> bool {
    powershell_command_syntax_error(command).is_some()
}

/// Bounded diagnostics from the same cached parse used for admission. `None`
/// means no positive syntax-error report, not proof that execution is safe.
pub fn powershell_command_syntax_error(command: &[String]) -> Option<String> {
    let (executable, args) = command.split_first()?;
    let PowershellInvocation::InlineCommand { script, .. } = parse_powershell_invocation(args)
    else {
        return None;
    };
    match parse_with_powershell_ast(executable, script) {
        PowershellParseOutcome::SyntaxError(diagnostic) => Some(diagnostic),
        _ => None,
    }
}

fn parse_with_powershell_ast_request(
    executable: &str,
    script: &str,
    resolution: Option<&PowershellResolutionState>,
) -> PowershellParseOutcome {
    if let Some(scope) = PowershellAnalysisScope::current() {
        let Some((entry, syntax)) = scope.syntax_for(executable, script, |deadline| {
            parse_with_powershell_ast_before(executable, script, None, deadline)
        }) else { return PowershellParseOutcome::Failed; };
        if entry.unavailable.load(std::sync::atomic::Ordering::Acquire) {
            return PowershellParseOutcome::Failed;
        }
        // Non-candidates need no live resolution; never reuse a resolved
        // application from another caller's cwd or environment.
        if resolution.is_none() || !matches!(&syntax,
            PowershellParseOutcome::Analysis(PowershellParseAnalysis { direct_argv: Some(_), .. }))
        {
            return syntax;
        }
        let resolved = parse_with_powershell_ast_before(executable, script, resolution, entry.deadline);
        if matches!(&resolved, PowershellParseOutcome::Failed) {
            entry.unavailable.store(true, std::sync::atomic::Ordering::Release);
        }
        return resolved;
    }
    let deadline = Instant::now() + POWERSHELL_PARSER_RESPONSE_TIMEOUT;
    parse_with_powershell_ast_before(executable, script, resolution, deadline)
}

fn parse_with_powershell_ast_before(
    executable: &str,
    script: &str,
    resolution: Option<&PowershellResolutionState>,
    deadline: Instant,
) -> PowershellParseOutcome {
    let Some(flavor) = PowershellFlavor::from_requested_executable(executable) else {
        return PowershellParseOutcome::Failed;
    };
    let parser = {
        let mut parser_processes = PARSER_PROCESSES
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        parser_processes
            .entry(flavor)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };
    let access = match acquire_cached_parser(&parser, SPARE_PARSERS.as_slice()) {
        CachedParserAccess::Saturated => {
            wait_for_cached_parser(&parser, SPARE_PARSERS.as_slice(), deadline)
                .unwrap_or(CachedParserAccess::Saturated)
        }
        access => access,
    };
    let outcome = match access {
        CachedParserAccess::Shared(mut parser) => {
            parse_with_cached_process(&mut parser, executable, script, resolution, deadline)
        }
        CachedParserAccess::Spare(mut parsers) => {
            // Retain the overflow host and its syntax cache across bursts instead
            // of paying PowerShell startup on every concurrent classification.
            let parser = parsers.entry(flavor).or_default();
            parse_with_cached_process(parser, executable, script, resolution, deadline)
        }
        CachedParserAccess::Saturated => PowershellParseOutcome::Failed,
    };
    notify_parser_available();
    outcome
}

enum CachedParserAccess<'a> {
    Shared(MutexGuard<'a, Option<PowershellParserProcess>>),
    Spare(MutexGuard<'a, SpareParsers>),
    Saturated,
}

fn acquire_cached_parser<'a>(
    parser: &'a CachedParser,
    spare_parsers: &'a [Mutex<SpareParsers>],
) -> CachedParserAccess<'a> {
    match parser.try_lock() {
        Ok(parser) => return CachedParserAccess::Shared(parser),
        Err(TryLockError::Poisoned(poisoned)) => {
            return CachedParserAccess::Shared(poisoned.into_inner());
        }
        Err(TryLockError::WouldBlock) => {}
    }
    for spare in spare_parsers {
        match spare.try_lock() {
            Ok(slot) => return CachedParserAccess::Spare(slot),
            Err(TryLockError::Poisoned(poisoned)) => {
                return CachedParserAccess::Spare(poisoned.into_inner());
            }
            Err(TryLockError::WouldBlock) => {}
        }
    }
    CachedParserAccess::Saturated
}

pub(crate) fn try_parse_powershell_ast_commands(
    executable: &str,
    script: &str,
) -> Option<Vec<Vec<String>>> {
    match parse_with_powershell_ast(executable, script) {
        PowershellParseOutcome::Analysis(analysis) => Some(analysis.commands),
        PowershellParseOutcome::Unsupported
        | PowershellParseOutcome::Failed
        | PowershellParseOutcome::SyntaxError(_) => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PowershellDirectArgvCandidate {
    pub argv: Vec<String>,
    pub native_argument_mode: String,
    pub powershell_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PowershellParseAnalysis {
    pub commands: Vec<Vec<String>>,
    pub standalone_command: bool,
    pub direct_argv: Option<PowershellDirectArgvCandidate>,
    /// Resource inspection only; not a direct-execution or runner-receipt proof.
    pub inspection_argv: Option<Vec<String>>,
    pub resolved_application: Option<String>,
}

pub(crate) fn try_parse_powershell_ast_analysis(
    executable: &str,
    script: &str,
) -> Option<PowershellParseAnalysis> {
    match parse_with_powershell_ast(executable, script) {
        PowershellParseOutcome::Analysis(analysis) => Some(analysis),
        PowershellParseOutcome::Unsupported
        | PowershellParseOutcome::Failed
        | PowershellParseOutcome::SyntaxError(_) => None,
    }
}

pub(crate) fn try_parse_powershell_ast_analysis_with_resolution(
    executable: &str,
    script: &str,
    resolution: &PowershellResolutionState,
) -> Option<PowershellParseAnalysis> {
    match parse_with_powershell_ast_request(executable, script, Some(resolution)) {
        PowershellParseOutcome::Analysis(analysis) => Some(analysis),
        PowershellParseOutcome::Unsupported
        | PowershellParseOutcome::Failed
        | PowershellParseOutcome::SyntaxError(_) => None,
    }
}

pub(crate) fn is_trusted_powershell_host(executable: &str) -> bool {
    let Some(flavor) = PowershellFlavor::from_requested_executable(executable) else {
        return false;
    };
    let Some(trusted_executable) = trusted_parser_executable(flavor) else {
        return false;
    };
    requested_executable_matches_trusted(executable, &trusted_executable)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PowershellParseOutcome {
    Analysis(PowershellParseAnalysis),
    Unsupported,
    SyntaxError(String),
    Failed,
}

fn parse_with_cached_process(
    parser_process: &mut Option<PowershellParserProcess>,
    executable: &str,
    script: &str,
    resolution: Option<&PowershellResolutionState>,
    deadline: Instant,
) -> PowershellParseOutcome {
    for attempt in 0..=1 {
        if Instant::now() >= deadline {
            return PowershellParseOutcome::Failed;
        }
        if parser_process.is_none() {
            match PowershellParserProcess::spawn(executable) {
                Ok(process) => {
                    *parser_process = Some(process);
                }
                Err(_) => return PowershellParseOutcome::Failed,
            }
        }

        let Some(process) = parser_process.as_mut() else {
            return PowershellParseOutcome::Failed;
        };
        process.observation.reference();
        let parse_result = process.parse_request_before(script, resolution, deadline);
        match parse_result {
            Ok(outcome) => return outcome,
            Err(error) => {
                // The common failure mode here is that a previously cached child exited or its
                // stdio stream became unusable between requests. Drop that process and retry once
                // with a fresh child before giving up. A timed-out child is forcibly terminated by
                // `parse`; fail closed immediately instead of spending another deadline retrying.
                let timed_out = error.kind() == ErrorKind::TimedOut;
                *parser_process = None;
                if timed_out || attempt == 1 {
                    return PowershellParseOutcome::Failed;
                }
            }
        }
    }

    PowershellParseOutcome::Failed
}

fn encode_powershell_base64(script: &str) -> String {
    let mut utf16 = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    BASE64_STANDARD.encode(utf16)
}

const PARSER_SOURCE_ENV: &str = "CODEX_INTERNAL_POWERSHELL_PARSER_SOURCE";
const PARSER_BOOTSTRAP: &str = concat!(
    "$s=$env:",
    "CODEX_INTERNAL_POWERSHELL_PARSER_SOURCE",
    ";Remove-Item Env:",
    "CODEX_INTERNAL_POWERSHELL_PARSER_SOURCE",
    ";Invoke-Expression ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($s)))"
);

fn encoded_parser_source() -> &'static str {
    static ENCODED: LazyLock<String> =
        LazyLock::new(|| BASE64_STANDARD.encode(POWERSHELL_PARSER_SCRIPT.as_bytes()));
    &ENCODED
}

fn encoded_parser_bootstrap() -> &'static str {
    static ENCODED: LazyLock<String> = LazyLock::new(|| encode_powershell_base64(PARSER_BOOTSTRAP));
    &ENCODED
}

struct PowershellParserProcess {
    observation: codex_utils_pty::process_observation::ObservedProcess,
    child: Option<Child>,
    requests: mpsc::Sender<ParserIoRequest>,
    // Syntax is deterministic within this host. Keep just the latest bounded result so
    // consecutive safety consumers do not repeat the same AST parse and IPC round trip.
    syntax_cache: VecDeque<(String, PowershellParseOutcome, usize)>,
    // Request ids are monotonic within one child process so the caller can detect protocol
    // desynchronization if stdout is contaminated or the child is unexpectedly replaced.
    next_request_id: u64,
}

impl PowershellParserProcess {
    fn spawn(executable: &str) -> std::io::Result<Self> {
        let flavor = PowershellFlavor::from_requested_executable(executable).ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidInput,
                "unsupported PowerShell executable name",
            )
        })?;
        let trusted_executable = trusted_parser_executable(flavor).ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::NotFound,
                "no trusted PowerShell parser host is installed",
            )
        })?;
        let trusted_working_directory = trusted_executable.parent().ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidData,
                "trusted PowerShell parser host has no parent directory",
            )
        })?;
        #[cfg(windows)]
        let child = windows_process::spawn(&trusted_executable, trusted_working_directory)?;
        #[cfg(not(windows))]
        let child = Command::new(&trusted_executable)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                encoded_parser_bootstrap(),
            ])
            .env(PARSER_SOURCE_ENV, encoded_parser_source())
            .current_dir(trusted_working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        #[cfg(windows)]
        let observation = child.observation.clone();
        #[cfg(not(windows))]
        let observation = codex_utils_pty::process_observation::observe_process_start(
            "powershell_parser",
            codex_utils_pty::process_observation::ProcessRole::AuxiliaryRoot,
            Some(child.id()),
            None,
        );
        let mut child = Some(child);
        let stdin_result = child
            .as_mut()
            .ok_or_else(|| {
                std::io::Error::new(
                    ErrorKind::BrokenPipe,
                    "PowerShell parser child was unavailable",
                )
            })
            .and_then(take_child_stdin);
        let stdin = match stdin_result {
            Ok(stdin) => stdin,
            Err(error) => {
                kill_child(&mut child);
                return Err(error);
            }
        };
        let stdout_result = child
            .as_mut()
            .ok_or_else(|| {
                std::io::Error::new(
                    ErrorKind::BrokenPipe,
                    "PowerShell parser child was unavailable",
                )
            })
            .and_then(take_child_stdout);
        let stdout = match stdout_result {
            Ok(stdout) => stdout,
            Err(error) => {
                kill_child(&mut child);
                return Err(error);
            }
        };
        let requests = match spawn_parser_io_worker(stdin, stdout) {
            Ok(requests) => requests,
            Err(error) => {
                kill_child(&mut child);
                return Err(error);
            }
        };
        Ok(Self {
            observation,
            child,
            requests,
            syntax_cache: VecDeque::new(),
            next_request_id: 0,
        })
    }



    fn parse_request_before(
        &mut self,
        script: &str,
        resolution: Option<&PowershellResolutionState>,
        deadline: Instant,
    ) -> std::io::Result<PowershellParseOutcome> {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "PowerShell parser deadline expired",
            ));
        }
        if let Some(index) = self.syntax_cache.iter().position(|(cached, outcome, _)| {
            cached == script
                && (resolution.is_none()
                    || matches!(
                        outcome,
                        PowershellParseOutcome::Unsupported
                            | PowershellParseOutcome::SyntaxError(_)
                            | PowershellParseOutcome::Analysis(PowershellParseAnalysis {
                                direct_argv: None,
                                ..
                            })
                    ))
        }) && let Some(entry) = self.syntax_cache.remove(index)
        {
            let outcome = entry.1.clone();
            self.syntax_cache.push_back(entry);
            return Ok(outcome);
        }
        // A syntax result with no direct-argv candidate cannot perform resolution.
        // Candidates still depend on the current filesystem, cwd, PATH and PATHEXT:
        // their resolution must reach the host and must not enter this cache.
        // The resolution script restores cwd/PATH/PATHEXT and does not change
        // syntax or native argument mode. Keep syntax proof across those requests.
        let request = PowershellParserRequest {
            id: self.next_request_id,
            payload: encode_powershell_base64(script),
            resolution: resolution.cloned(),
        };
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let mut request_json = serialize_request(&request)?;
        request_json.push('\n');
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        self.requests
            .send(ParserIoRequest {
                request_json,
                response_tx,
            })
            .map_err(|_| {
                std::io::Error::new(ErrorKind::BrokenPipe, "PowerShell parser worker exited")
            })?;
        let response_line =
            match response_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(result) => result?,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    kill_child(&mut self.child);
                    return Err(std::io::Error::new(
                        ErrorKind::TimedOut,
                        "PowerShell parser response timed out",
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(std::io::Error::new(
                        ErrorKind::BrokenPipe,
                        "PowerShell parser worker disconnected",
                    ));
                }
            };

        let response = deserialize_response(&response_line)?;
        // Requests are serialized today; the id still catches protocol desyncs if stdout is
        // contaminated or the child process is unexpectedly replaced mid-request. That turns an
        // ambiguous parser result into a hard failure so the caller can discard the cached child.
        if response.id != request.id {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "PowerShell parser returned response id {} for request {}",
                    response.id, request.id
                ),
            ));
        }

        let syntax_error = response.status == "parse_errors";
        let outcome = response.into_outcome();
        if resolution.is_none()
            && (syntax_error || !matches!(outcome, PowershellParseOutcome::Failed))
            && script.len().saturating_add(response_line.len()) <= MAX_CACHED_SYNTAX_BYTES
        {
            let size = script.len().saturating_add(response_line.len());
            while self.syntax_cache.len() >= MAX_CACHED_SYNTAX_ENTRIES
                || self
                    .syntax_cache
                    .iter()
                    .map(|(_, _, size)| size)
                    .sum::<usize>()
                    + size
                    > MAX_CACHED_SYNTAX_BYTES
            {
                self.syntax_cache.pop_front();
            }
            self.syntax_cache
                .push_back((script.to_string(), outcome.clone(), size));
        }
        Ok(outcome)
    }
}

struct ParserIoRequest {
    request_json: String,
    response_tx: mpsc::SyncSender<std::io::Result<String>>,
}

fn spawn_parser_io_worker(
    mut stdin: ChildStdin,
    mut stdout: BufReader<ChildStdout>,
) -> std::io::Result<mpsc::Sender<ParserIoRequest>> {
    let (request_tx, request_rx) = mpsc::channel::<ParserIoRequest>();
    std::thread::Builder::new()
        .name("powershell-parser-io".to_string())
        .spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let result = (|| {
                    stdin.write_all(request.request_json.as_bytes())?;
                    stdin.flush()?;
                    let mut response_line = String::new();
                    if stdout.read_line(&mut response_line)? == 0 {
                        return Err(std::io::Error::new(
                            ErrorKind::UnexpectedEof,
                            "PowerShell parser closed stdout",
                        ));
                    }
                    Ok(response_line)
                })();
                let failed = result.is_err();
                let _ = request.response_tx.send(result);
                if failed {
                    break;
                }
            }
        })?;
    Ok(request_tx)
}

impl PowershellFlavor {
    fn from_requested_executable(executable: &str) -> Option<Self> {
        let name = Path::new(executable)
            .file_name()
            .and_then(|name| name.to_str())?
            .to_ascii_lowercase();
        match name.as_str() {
            "powershell" | "powershell.exe" => Some(Self::WindowsPowerShell),
            "pwsh" | "pwsh.exe" => Some(Self::Pwsh),
            _ => None,
        }
    }
}

fn requested_executable_matches_trusted(executable: &str, trusted: &Path) -> bool {
    let requested = Path::new(executable);
    if requested.is_absolute() || requested.components().count() > 1 {
        return windows_explicit_path_matches_trusted(requested, trusted);
    }

    {
        // Windows executable lookup checks the application directory and current directory before
        // PATH. Reject a same-named shadow executable there even when PATH resolves to the trusted
        // PowerShell installation.
        if let Some(application_dir) = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            && let Some(matches) =
                existing_path_matches_trusted(&application_dir.join(requested), trusted)
        {
            return matches;
        }
        if let Ok(current_dir) = std::env::current_dir()
            && let Some(matches) =
                existing_path_matches_trusted(&current_dir.join(requested), trusted)
        {
            return matches;
        }
    }

    which::which(executable)
        .ok()
        .filter(|path| !windows_path_is_remote_or_device(path))
        .and_then(|path| fs::canonicalize(path).ok())
        .is_some_and(|path| path == trusted)
}

fn windows_explicit_path_matches_trusted(requested: &Path, trusted: &Path) -> bool {
    if windows_path_is_remote_or_device(requested) {
        return false;
    }
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        let Ok(current_dir) = std::env::current_dir() else {
            return false;
        };
        current_dir.join(requested)
    };
    let trusted_key = windows_path_lookup_key(trusted);
    if windows_path_lookup_key(&requested) == trusted_key {
        return true;
    }
    if requested.extension().is_none() {
        let mut with_exe = requested;
        with_exe.set_extension("exe");
        return windows_path_lookup_key(&with_exe) == trusted_key;
    }
    false
}

fn windows_path_is_remote_or_device(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().starts_with(r"\\")
}

fn windows_path_lookup_key(path: &Path) -> String {
    let text = path.as_os_str().to_string_lossy().replace('/', "\\");
    text.strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn existing_path_matches_trusted(path: &Path, trusted: &Path) -> Option<bool> {
    if windows_path_is_remote_or_device(path) {
        return Some(false);
    }
    let mut candidates = vec![path.to_path_buf()];

    if path.extension().is_none() {
        let mut with_exe = path.to_path_buf();
        with_exe.set_extension("exe");
        candidates.push(with_exe);
    }

    let canonical_candidates: Vec<PathBuf> = candidates
        .into_iter()
        .filter_map(|candidate| fs::canonicalize(candidate).ok())
        .filter(|candidate| candidate.is_file())
        .collect();
    (!canonical_candidates.is_empty()).then(|| {
        canonical_candidates
            .iter()
            .all(|candidate| candidate == trusted)
    })
}

fn trusted_parser_executable(flavor: PowershellFlavor) -> Option<PathBuf> {
    match flavor {
        PowershellFlavor::WindowsPowerShell => trusted_executable_under(
            PathBuf::from(std::env::var_os("SystemRoot")?),
            &["System32", "WindowsPowerShell", "v1.0", "powershell.exe"],
        ),
        PowershellFlavor::Pwsh => ["ProgramW6432", "ProgramFiles"]
            .into_iter()
            .filter_map(std::env::var_os)
            .find_map(|root| {
                trusted_executable_under(PathBuf::from(root), &["PowerShell", "7", "pwsh.exe"])
            }),
    }
}

fn trusted_executable_under(root: PathBuf, relative_components: &[&str]) -> Option<PathBuf> {
    if !root.is_absolute() || windows_path_is_remote_or_device(&root) {
        return None;
    }
    let canonical_root = fs::canonicalize(root).ok()?;
    let mut candidate = canonical_root.clone();
    candidate.extend(relative_components);
    let candidate = fs::canonicalize(candidate).ok()?;
    (candidate.is_file() && candidate.starts_with(canonical_root)).then_some(candidate)
}

impl Drop for PowershellParserProcess {
    fn drop(&mut self) {
        kill_child(&mut self.child);
    }
}

fn take_child_stdin(child: &mut Child) -> std::io::Result<ChildStdin> {
    child.stdin.take().ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::BrokenPipe,
            "PowerShell parser child did not expose stdin",
        )
    })
}

fn take_child_stdout(child: &mut Child) -> std::io::Result<BufReader<ChildStdout>> {
    child.stdout.take().map(BufReader::new).ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::BrokenPipe,
            "PowerShell parser child did not expose stdout",
        )
    })
}

fn serialize_request(request: &PowershellParserRequest) -> std::io::Result<String> {
    serde_json::to_string(request).map_err(|error| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!("failed to serialize PowerShell parser request: {error}"),
        )
    })
}

fn deserialize_response(response_line: &str) -> std::io::Result<PowershellParserResponse> {
    serde_json::from_str(response_line).map_err(|error| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!("failed to parse PowerShell parser response: {error}"),
        )
    })
}

#[derive(Serialize)]
struct PowershellParserRequest {
    id: u64,
    payload: String,
    resolution: Option<PowershellResolutionState>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PowershellParserResponse {
    id: u64,
    status: String,
    commands: Option<Vec<Vec<String>>>,
    #[serde(default)]
    standalone_command: bool,
    direct_argv: Option<Vec<String>>,
    #[serde(default)]
    inspection_argv: Option<Vec<String>>,
    native_argument_mode: Option<String>,
    powershell_version: Option<String>,
    resolved_application: Option<String>,
    syntax_error: Option<String>,
}

impl PowershellParserResponse {
    fn into_outcome(self) -> PowershellParseOutcome {
        match self.status.as_str() {
            "ok" => self
                .commands
                .filter(|commands| {
                    !commands.is_empty()
                        && commands
                            .iter()
                            .all(|cmd| !cmd.is_empty() && !cmd[0].is_empty())
                })
                .map(|commands| {
                    let inspection_argv = self.inspection_argv.filter(|argv| {
                        self.standalone_command
                            && commands.len() == 1
                            && !argv.is_empty()
                            && !argv[0].is_empty()
                            && matches!(
                                self.native_argument_mode.as_deref(),
                                Some("Standard" | "Windows")
                            )
                            && self
                                .powershell_version
                                .as_deref()
                                .is_some_and(|version| version.split('.').next() == Some("7"))
                    });
                    let direct_argv = self
                        .direct_argv
                        .filter(|argv| !argv.is_empty() && !argv[0].is_empty())
                        .zip(self.native_argument_mode.zip(self.powershell_version))
                        .filter(|(_, (mode, version))| {
                            matches!(mode.as_str(), "Standard" | "Windows")
                                && version.split('.').next() == Some("7")
                        })
                        .map(|(argv, (native_argument_mode, powershell_version))| {
                            PowershellDirectArgvCandidate {
                                argv,
                                native_argument_mode,
                                powershell_version,
                            }
                        });
                    PowershellParseOutcome::Analysis(PowershellParseAnalysis {
                        standalone_command: self.standalone_command && commands.len() == 1,
                        commands,
                        direct_argv,
                        inspection_argv,
                        resolved_application: self.resolved_application,
                    })
                })
                .unwrap_or(PowershellParseOutcome::Unsupported),
            "unsupported" => PowershellParseOutcome::Unsupported,
            "parse_errors" => {
                PowershellParseOutcome::SyntaxError(self.syntax_error.unwrap_or_default())
            }
            _ => PowershellParseOutcome::Failed,
        }
    }
}

fn kill_child(child: &mut Option<Child>) {
    let Some(mut child) = child.take() else {
        return;
    };
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let _ = child.kill();
    // Waiting synchronously here would defeat the parser response deadline if termination itself
    // stalls. Reap in the background so the caller can immediately discard and replace this host.
    let _ = std::thread::Builder::new()
        .name("powershell-parser-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
}

