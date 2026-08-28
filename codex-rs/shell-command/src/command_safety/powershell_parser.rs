use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::mpsc;
use std::time::Duration;

const POWERSHELL_PARSER_SCRIPT: &str = include_str!("powershell_parser.ps1");
const POWERSHELL_PARSER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum PowershellFlavor {
    WindowsPowerShell,
    Pwsh,
}

type CachedParser = Arc<Mutex<Option<PowershellParserProcess>>>;

static PARSER_PROCESSES: LazyLock<Mutex<HashMap<PowershellFlavor, CachedParser>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PowershellResolutionState {
    pub cwd: String,
    pub path: String,
    pub pathext: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PowershellInvocation<'a> {
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
pub(super) fn parse_powershell_invocation(args: &[String]) -> PowershellInvocation<'_> {
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
                return PowershellInvocation::InlineCommand { script, no_profile };
            }
            _ if lower.starts_with("-command:") || lower.starts_with("/command:") => {
                if idx + 1 != args.len() {
                    return PowershellInvocation::Invalid;
                }
                let Some((_, script)) = arg.split_once(':') else {
                    return PowershellInvocation::Invalid;
                };
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

fn parse_with_powershell_ast_request(
    executable: &str,
    script: &str,
    resolution: Option<&PowershellResolutionState>,
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
    let mut parser = lock_cached_parser(&parser);
    parse_with_cached_process(&mut parser, executable, script, resolution)
}

fn lock_cached_parser(parser: &CachedParser) -> MutexGuard<'_, Option<PowershellParserProcess>> {
    parser.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn try_parse_powershell_ast_commands(
    executable: &str,
    script: &str,
) -> Option<Vec<Vec<String>>> {
    match parse_with_powershell_ast(executable, script) {
        PowershellParseOutcome::Analysis(analysis) => Some(analysis.commands),
        PowershellParseOutcome::Unsupported | PowershellParseOutcome::Failed => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PowershellDirectArgvCandidate {
    pub argv: Vec<String>,
    pub native_argument_mode: String,
    pub powershell_version: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PowershellParseAnalysis {
    pub commands: Vec<Vec<String>>,
    pub direct_argv: Option<PowershellDirectArgvCandidate>,
    pub resolved_application: Option<String>,
}

pub(crate) fn try_parse_powershell_ast_analysis(
    executable: &str,
    script: &str,
) -> Option<PowershellParseAnalysis> {
    match parse_with_powershell_ast(executable, script) {
        PowershellParseOutcome::Analysis(analysis) => Some(analysis),
        PowershellParseOutcome::Unsupported | PowershellParseOutcome::Failed => None,
    }
}

pub(crate) fn try_parse_powershell_ast_analysis_with_resolution(
    executable: &str,
    script: &str,
    resolution: &PowershellResolutionState,
) -> Option<PowershellParseAnalysis> {
    match parse_with_powershell_ast_request(executable, script, Some(resolution)) {
        PowershellParseOutcome::Analysis(analysis) => Some(analysis),
        PowershellParseOutcome::Unsupported | PowershellParseOutcome::Failed => None,
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

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PowershellParseOutcome {
    Analysis(PowershellParseAnalysis),
    Unsupported,
    Failed,
}

fn parse_with_cached_process(
    parser_process: &mut Option<PowershellParserProcess>,
    executable: &str,
    script: &str,
    resolution: Option<&PowershellResolutionState>,
) -> PowershellParseOutcome {
    for attempt in 0..=1 {
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
        let parse_result = process.parse_request(script, resolution);
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
    child: Option<Child>,
    requests: mpsc::Sender<ParserIoRequest>,
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
            child,
            requests,
            next_request_id: 0,
        })
    }

    #[cfg(test)]
    fn parse(&mut self, script: &str) -> std::io::Result<PowershellParseOutcome> {
        self.parse_request(script, None)
    }

    fn parse_request(
        &mut self,
        script: &str,
        resolution: Option<&PowershellResolutionState>,
    ) -> std::io::Result<PowershellParseOutcome> {
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
        let response_line = match response_rx.recv_timeout(POWERSHELL_PARSER_RESPONSE_TIMEOUT) {
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

        Ok(response.into_outcome())
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
    direct_argv: Option<Vec<String>>,
    native_argument_mode: Option<String>,
    powershell_version: Option<String>,
    resolved_application: Option<String>,
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
                        commands,
                        direct_argv,
                        resolved_application: self.resolved_application,
                    })
                })
                .unwrap_or(PowershellParseOutcome::Unsupported),
            "unsupported" => PowershellParseOutcome::Unsupported,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::powershell::try_find_powershell_executable_blocking;
    use pretty_assertions::assert_eq;

    fn parser_response_with_mode(mode: &str, version: &str) -> PowershellParseOutcome {
        PowershellParserResponse {
            id: 1,
            status: "ok".to_string(),
            commands: Some(vec![vec!["python".to_string()]]),
            direct_argv: Some(vec!["python".to_string()]),
            native_argument_mode: Some(mode.to_string()),
            powershell_version: Some(version.to_string()),
            resolved_application: None,
        }
        .into_outcome()
    }

    #[test]
    fn cached_parser_contention_queues_instead_of_creating_an_uncached_host() {
        let parser: CachedParser = Arc::new(Mutex::new(None));
        let held = lock_cached_parser(&parser);
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let queued_parser = Arc::clone(&parser);
        let queued = std::thread::spawn(move || {
            let _guard = lock_cached_parser(&queued_parser);
            acquired_tx.send(()).expect("report queued acquisition");
        });

        assert!(
            acquired_rx.recv_timeout(Duration::from_millis(25)).is_err(),
            "contended parser access did not queue"
        );
        drop(held);
        acquired_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("queued parser access should resume");
        queued.join().expect("queued parser thread");
    }

    #[test]
    fn direct_candidate_requires_a_supported_native_argument_mode() {
        for (mode, version) in [
            ("Legacy", "7.5.0"),
            ("Standard", "5.1.0"),
            ("Future", "8.0.0"),
        ] {
            let PowershellParseOutcome::Analysis(analysis) =
                parser_response_with_mode(mode, version)
            else {
                panic!("expected parser analysis");
            };
            assert_eq!(analysis.direct_argv, None);
        }

        let PowershellParseOutcome::Analysis(analysis) =
            parser_response_with_mode("Standard", "7.5.0")
        else {
            panic!("expected parser analysis");
        };
        assert!(analysis.direct_argv.is_some());
    }

    #[test]
    fn parser_process_handles_multiple_requests() {
        let Some(powershell) = try_find_powershell_executable_blocking() else {
            return;
        };
        let powershell = powershell.as_path().to_str().unwrap();
        let mut parser = PowershellParserProcess::spawn(powershell).unwrap();

        let first = parser.parse("Get-Content 'foo bar'").unwrap();
        let PowershellParseOutcome::Analysis(first) = first else {
            panic!("expected parser analysis");
        };
        assert_eq!(
            first.commands,
            vec![vec!["Get-Content".to_string(), "foo bar".to_string(),]],
        );

        let second = parser.parse("Write-Output foo | Measure-Object").unwrap();
        let PowershellParseOutcome::Analysis(second) = second else {
            panic!("expected parser analysis");
        };
        assert_eq!(
            second.commands,
            vec![
                vec!["Write-Output".to_string(), "foo".to_string()],
                vec!["Measure-Object".to_string()],
            ],
        );
    }

    #[test]
    fn parser_cache_contention_does_not_queue_classification() {
        let Some(powershell) = try_find_powershell_executable_blocking() else {
            return;
        };
        let powershell = powershell.as_path().to_str().unwrap().to_string();
        let flavor = PowershellFlavor::from_requested_executable(&powershell).unwrap();
        let cached_parser = {
            let mut parser_processes = PARSER_PROCESSES
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            parser_processes
                .entry(flavor)
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        let cached_parser_guard = cached_parser.lock().unwrap_or_else(PoisonError::into_inner);
        let (result_tx, result_rx) = mpsc::sync_channel(1);

        std::thread::spawn(move || {
            let result = parse_with_powershell_ast(&powershell, "Get-Content Cargo.toml");
            let _ = result_tx.send(result);
        });

        let result = result_rx
            .recv_timeout(POWERSHELL_PARSER_RESPONSE_TIMEOUT + Duration::from_secs(5))
            .expect("classification must not wait for the occupied cached parser");
        drop(cached_parser_guard);
        assert!(matches!(result, PowershellParseOutcome::Analysis(_)));
    }

    #[test]
    fn parser_process_rejects_stop_parsing_forms() {
        let Some(powershell) = try_find_powershell_executable_blocking() else {
            return;
        };
        let powershell = powershell.as_path().to_str().unwrap();
        let mut parser = PowershellParserProcess::spawn(powershell).unwrap();

        let parsed = parser
            .parse("git log --% HEAD --output=codex_poc.txt")
            .unwrap();
        assert_eq!(parsed, PowershellParseOutcome::Unsupported);
    }

    #[test]
    fn parser_process_rejects_param_blocks() {
        let Some(powershell) = try_find_powershell_executable_blocking() else {
            return;
        };
        let powershell = powershell.as_path().to_str().unwrap();
        let mut parser = PowershellParserProcess::spawn(powershell).unwrap();

        let parsed = parser
            .parse("param([string]$path = (Get-Location)) Write-Output test")
            .unwrap();
        assert_eq!(parsed, PowershellParseOutcome::Unsupported);
    }

    #[test]
    fn parser_process_rejects_named_blocks() {
        let Some(powershell) = try_find_powershell_executable_blocking() else {
            return;
        };
        let powershell = powershell.as_path().to_str().unwrap();
        let mut parser = PowershellParserProcess::spawn(powershell).unwrap();

        let parsed = parser
            .parse("begin { Set-Content codex_poc.txt pwned } end { Get-Content Cargo.toml }")
            .unwrap();
        assert_eq!(parsed, PowershellParseOutcome::Unsupported);
    }

    #[test]
    fn parser_process_rejects_using_statements() {
        let Some(powershell) = try_find_powershell_executable_blocking() else {
            return;
        };
        let powershell = powershell.as_path().to_str().unwrap();
        let mut parser = PowershellParserProcess::spawn(powershell).unwrap();

        let parsed = parser
            .parse("using module ./codex_poc.psm1\nGet-Content Cargo.toml")
            .unwrap();
        assert_eq!(parsed, PowershellParseOutcome::Unsupported);
    }

    #[test]
    fn parser_process_rejects_trap_blocks() {
        let Some(powershell) = try_find_powershell_executable_blocking() else {
            return;
        };
        let powershell = powershell.as_path().to_str().unwrap();
        let mut parser = PowershellParserProcess::spawn(powershell).unwrap();

        let parsed = parser
            .parse(
                "trap { Set-Content codex_poc.txt pwned; continue } Get-Content missing -ErrorAction Stop",
            )
            .unwrap();
        assert_eq!(parsed, PowershellParseOutcome::Unsupported);
    }
}
