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
use std::sync::PoisonError;
use std::sync::mpsc;
use std::time::Duration;

const POWERSHELL_PARSER_SCRIPT: &str = include_str!("powershell_parser.ps1");
const POWERSHELL_PARSER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(windows))]
const TRUSTED_PWSH_ROOTS: &[&str] = &[
    "/usr/bin",
    "/usr/local/microsoft/powershell",
    "/opt/microsoft/powershell",
    "/opt/homebrew",
    "/nix/store",
    "/snap",
];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum PowershellFlavor {
    WindowsPowerShell,
    Pwsh,
}

type CachedParser = Arc<Mutex<Option<PowershellParserProcess>>>;

/// Cache one long-lived parser process per trusted PowerShell flavor. The map lock only protects
/// cache lookup; each parser has its own lock so a stalled host cannot block the other flavor.
pub(super) fn parse_with_powershell_ast(executable: &str, script: &str) -> PowershellParseOutcome {
    static PARSER_PROCESSES: LazyLock<Mutex<HashMap<PowershellFlavor, CachedParser>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

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
    let mut parser = parser.lock().unwrap_or_else(PoisonError::into_inner);
    parse_with_cached_process(&mut parser, executable, script)
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
    Commands(Vec<Vec<String>>),
    Unsupported,
    Failed,
}

fn parse_with_cached_process(
    parser_process: &mut Option<PowershellParserProcess>,
    executable: &str,
    script: &str,
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
        let parse_result = process.parse(script);
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

fn encoded_parser_script() -> &'static str {
    static ENCODED: LazyLock<String> =
        LazyLock::new(|| encode_powershell_base64(POWERSHELL_PARSER_SCRIPT));
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
                encoded_parser_script(),
            ])
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

    fn parse(&mut self, script: &str) -> std::io::Result<PowershellParseOutcome> {
        let request = PowershellParserRequest {
            id: self.next_request_id,
            payload: encode_powershell_base64(script),
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
        #[cfg(windows)]
        return windows_explicit_path_matches_trusted(requested, trusted);

        #[cfg(not(windows))]
        return non_windows_explicit_path_matches_trusted(requested, trusted);
    }

    #[cfg(windows)]
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
        .filter(|path| {
            #[cfg(windows)]
            {
                !windows_path_is_remote_or_device(path)
            }
            #[cfg(not(windows))]
            {
                true
            }
        })
        .and_then(|path| fs::canonicalize(path).ok())
        .is_some_and(|path| path == trusted)
}

#[cfg(not(windows))]
fn non_windows_explicit_path_matches_trusted(requested: &Path, trusted: &Path) -> bool {
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        let Ok(current_dir) = std::env::current_dir() else {
            return false;
        };
        current_dir.join(requested)
    };
    if !TRUSTED_PWSH_ROOTS
        .iter()
        .any(|root| requested.starts_with(Path::new(root)))
    {
        return false;
    }
    existing_path_matches_trusted(&requested, trusted).unwrap_or(false)
}

#[cfg(windows)]
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

#[cfg(windows)]
fn windows_path_is_remote_or_device(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().starts_with(r"\\")
}

#[cfg(windows)]
fn windows_path_lookup_key(path: &Path) -> String {
    let text = path.as_os_str().to_string_lossy().replace('/', "\\");
    text.strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn existing_path_matches_trusted(path: &Path, trusted: &Path) -> Option<bool> {
    #[cfg(windows)]
    if windows_path_is_remote_or_device(path) {
        return Some(false);
    }
    let mut candidates = vec![path.to_path_buf()];
    #[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(not(windows))]
fn trusted_parser_executable(_flavor: PowershellFlavor) -> Option<PathBuf> {
    // PowerShell Core is the only supported host off Windows. Resolve it independently from the
    // executable being classified, canonicalize the result, and require a standard system/package
    // installation root. This preserves cross-platform command-preflight parsing without executing
    // a model-supplied path that merely has a PowerShell-looking basename.
    let candidate = fs::canonicalize(which::which("pwsh").ok()?).ok()?;
    let current_dir = fs::canonicalize(std::env::current_dir().ok()?).ok()?;
    let under_trusted_root = TRUSTED_PWSH_ROOTS
        .iter()
        .any(|root| candidate.starts_with(Path::new(root)));
    (candidate.is_absolute()
        && candidate.is_file()
        && under_trusted_root
        && !candidate.starts_with(current_dir))
    .then_some(candidate)
}

#[cfg(windows)]
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PowershellParserResponse {
    id: u64,
    status: String,
    commands: Option<Vec<Vec<String>>>,
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
                            .all(|cmd| !cmd.is_empty() && cmd.iter().all(|word| !word.is_empty()))
                })
                .map(PowershellParseOutcome::Commands)
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::powershell::try_find_powershell_executable_blocking;
    use pretty_assertions::assert_eq;

    #[test]
    fn parser_process_handles_multiple_requests() {
        let Some(powershell) = try_find_powershell_executable_blocking() else {
            return;
        };
        let powershell = powershell.as_path().to_str().unwrap();
        let mut parser = PowershellParserProcess::spawn(powershell).unwrap();

        let first = parser.parse("Get-Content 'foo bar'").unwrap();
        assert_eq!(
            first,
            PowershellParseOutcome::Commands(vec![vec![
                "Get-Content".to_string(),
                "foo bar".to_string(),
            ]]),
        );

        let second = parser.parse("Write-Output foo | Measure-Object").unwrap();
        assert_eq!(
            second,
            PowershellParseOutcome::Commands(vec![
                vec!["Write-Output".to_string(), "foo".to_string()],
                vec!["Measure-Object".to_string()],
            ]),
        );
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
