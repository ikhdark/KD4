use std::collections::HashMap;

use std::ffi::OsStr;

use std::fs;

use std::path::Component;

use std::path::Path;
use std::path::PathBuf;

use std::path::Prefix;

use codex_utils_absolute_path::AbsolutePathBuf;

pub use crate::command_safety::PowershellDirectArgvCandidate;

use crate::command_safety::PowershellInvocation;
use crate::command_safety::PowershellResolutionState;
use crate::command_safety::parse_powershell_invocation;
use crate::command_safety::try_parse_powershell_ast_analysis;

use crate::command_safety::try_parse_powershell_ast_analysis_with_resolution;
use crate::command_safety::try_parse_powershell_ast_commands;
use crate::shell_detect::ShellType;
use crate::shell_detect::detect_shell_type;

/// Prefixed command for powershell shell calls to request UTF-8 console output.
pub const UTF8_OUTPUT_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

pub fn prefix_powershell_script_with_utf8(command: &[String]) -> Vec<String> {
    let Some(extracted) = extract_powershell_command_details(command) else {
        return command.to_vec();
    };

    let trimmed = extracted.script.trim_start();
    // Using/parameter/advanced-script preambles have placement rules. When
    // comments, attributes or a BOM hide the insertion point, leave the source
    // intact rather than adding a statement or introducing a new scope.
    let first_word = trimmed
        .split(|ch: char| !ch.is_alphanumeric())
        .next()
        .unwrap_or_default();
    if trimmed.starts_with(['#', '[', '\u{feff}'])
        || trimmed.starts_with("<#")
        || [
            "using",
            "param",
            "dynamicparam",
            "begin",
            "process",
            "end",
            "clean",
        ]
        .iter()
        .any(|word| first_word.eq_ignore_ascii_case(word))
    {
        return command.to_vec();
    }
    let script = if trimmed.starts_with(UTF8_OUTPUT_PREFIX) {
        extracted.script.to_string()
    } else {
        format!("{UTF8_OUTPUT_PREFIX}{}", extracted.script)
    };

    let mut command = command.to_vec();
    command[extracted.script_index] = format!(
        "{}{}",
        &command[extracted.script_index][..extracted.script_offset],
        script
    );
    command
}

/// Clears a stale native exit code before the command runs.
pub const NATIVE_EXIT_CODE_PROLOGUE: &str = "$global:LASTEXITCODE = $null\n";

/// Reports the command's own native exit code. A command that failed without
/// one (not found, cmdlet error) still exits 1, and success still exits 0.
pub const NATIVE_EXIT_CODE_EPILOGUE: &str = "\n$codexCommandSucceeded = $?; $codexNativeExitCode = $global:LASTEXITCODE\nif (-not $codexCommandSucceeded) { if ($codexNativeExitCode -is [int] -and $codexNativeExitCode -ne 0) { exit $codexNativeExitCode }; exit 1 }";

/// `pwsh -Command` reports every failing native exit code as 1, which hides codes
/// such as a validation runner's 75 ("busy, pending"). When the original script is
/// one plain command line ending the spawned script, keep that command's exit code.
/// Apply only at launch, after approval and classification saw the original script.
/// Returns `None` when the script does not qualify or does not end `command`.
pub fn preserve_powershell_native_exit_code(
    command: &[String],
    original_script: &str,
) -> Option<Vec<String>> {
    if !is_plain_powershell_command_line(original_script) {
        return None;
    }
    let extracted = extract_powershell_command_details(command)?;
    let preamble = extracted.script.strip_suffix(original_script)?;
    let script = format!(
        "{preamble}{NATIVE_EXIT_CODE_PROLOGUE}{original_script}{NATIVE_EXIT_CODE_EPILOGUE}"
    );
    let mut command = command.to_vec();
    command[extracted.script_index] = format!(
        "{}{}",
        &command[extracted.script_index][..extracted.script_offset],
        script
    );
    Some(command)
}

/// One command invocation of bare words and literal single-quoted strings: no
/// expansion, grouping, operators, redirection, comments or statement breaks,
/// and no preamble keyword whose placement rules a prologue would break.
fn is_plain_powershell_command_line(script: &str) -> bool {
    let script = script.trim();
    let first_word = script
        .split(|ch: char| !ch.is_alphanumeric())
        .next()
        .unwrap_or_default();
    if script.is_empty()
        || script.starts_with('\u{feff}')
        || [
            "using",
            "param",
            "dynamicparam",
            "begin",
            "process",
            "end",
            "clean",
        ]
        .iter()
        .any(|word| first_word.eq_ignore_ascii_case(word))
    {
        return false;
    }
    let mut in_single_quote = false;
    for ch in script.chars() {
        if in_single_quote {
            match ch {
                '\'' => in_single_quote = false,
                '\n' | '\r' => return false,
                _ => {}
            }
            continue;
        }
        match ch {
            '\'' => in_single_quote = true,
            '"' | '`' | '$' | ';' | '|' | '&' | '<' | '>' | '(' | ')' | '{' | '}' | '@' | '#'
            | ',' | '[' | ']' | '\n' | '\r' => return false,
            _ => {}
        }
    }
    !in_single_quote
}

struct ExtractedPowershellCommand<'a> {
    shell: &'a str,
    script: &'a str,
    script_index: usize,
    script_offset: usize,
    no_profile: bool,
}

/// Extract the PowerShell script body from an invocation such as:
///
/// - ["pwsh", "-NoProfile", "-Command", "Get-ChildItem -Recurse | Select-String foo"]
/// - ["powershell.exe", "-Command", "Write-Host hi"]
/// - ["powershell", "-NoLogo", "-NoProfile", "-Command", "...script..."]
///
/// Returns (`shell`, `script`) when the first arg is a PowerShell executable and a
/// `-Command` (or `-c`) flag is present followed by a script string.
pub fn extract_powershell_command(command: &[String]) -> Option<(&str, &str)> {
    let extracted = extract_powershell_command_details(command)?;
    Some((extracted.shell, extracted.script))
}

/// Extract a PowerShell script only when profiles are disabled and the requested wrapper resolves
/// to the trusted host.
///
/// This is intended for user-facing summaries that would otherwise hide the executable that will
/// actually run. Syntax-only consumers should use [`extract_powershell_command`] instead.
pub(crate) fn extract_trusted_noprofile_powershell_command(
    command: &[String],
) -> Option<(&str, &str)> {
    let (shell, script) = extract_noprofile_powershell_command(command)?;
    is_trusted_powershell_executable(shell).then_some((shell, script))
}

/// Extract an exact-shape PowerShell command only when profiles are explicitly disabled.
pub fn extract_noprofile_powershell_command(command: &[String]) -> Option<(&str, &str)> {
    let extracted = extract_powershell_command_details(command)?;
    extracted
        .no_profile
        .then_some((extracted.shell, extracted.script))
}

/// Return whether this executable resolves to the independently selected trusted PowerShell host.
pub fn is_trusted_powershell_executable(executable: &str) -> bool {
    crate::command_safety::is_trusted_powershell_host(executable)
}

pub fn is_trusted_powershell_executable_in_state(
    executable: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
) -> bool {
    crate::command_safety::trusted_powershell_host_path(executable).is_some_and(|trusted| {
        native_executable_against_trusted(executable, cwd, env, &trusted).is_some()
    })
}

fn extract_powershell_command_details(
    command: &[String],
) -> Option<ExtractedPowershellCommand<'_>> {
    if command.len() < 2 {
        return None;
    }

    let shell = &command[0];
    if !matches!(
        detect_shell_type(PathBuf::from(shell)),
        Some(ShellType::PowerShell)
    ) {
        return None;
    }

    let PowershellInvocation::InlineCommand { script, no_profile } =
        parse_powershell_invocation(&command[1..])
    else {
        return None;
    };
    let script_index = command.len() - 1;
    Some(ExtractedPowershellCommand {
        shell,
        script,
        script_index,
        script_offset: command[script_index].len() - script.len(),
        no_profile,
    })
}

/// Parse the script body from a top-level PowerShell wrapper into argv-like commands.
///
/// This exact-shape parser is used by non-approval consumers such as command preflight. Approval
/// and execution-policy decisions must use the `-NoProfile` variant below.
pub fn parse_powershell_command_into_plain_commands(
    command: &[String],
) -> Option<Vec<Vec<String>>> {
    let extracted = extract_powershell_command_details(command)?;
    try_parse_powershell_ast_commands(extracted.shell, extracted.script)
}

/// Prove one literal command launch, without claiming native argv equivalence.
/// Numeric arguments can preserve launch count while remaining ineligible for
/// direct execution. This syntax-only proof must not authenticate runner receipts.
pub fn is_standalone_powershell_command(script: &str) -> bool {
    try_parse_powershell_ast_analysis("pwsh", script)
        .is_some_and(|analysis| analysis.standalone_command)
}

/// Inspect one literal native command, including canonical decimal integers.
/// This proof is only for resource admission: it must not authorize direct
/// execution rewrites or authenticate runner receipts.
pub fn standalone_powershell_inspection_argv(script: &str) -> Option<Vec<String>> {
    try_parse_powershell_ast_analysis("pwsh", script)?.inspection_argv
}

/// Parse an exact-shape PowerShell command only when profiles are disabled, as required by
/// approval and execution-policy decisions that depend on the parsed command being equivalent to
/// the command that will actually run.
pub fn parse_noprofile_powershell_command_into_plain_commands(
    command: &[String],
) -> Option<Vec<Vec<String>>> {
    let (shell, script) = extract_noprofile_powershell_command(command)?;
    if !is_trusted_powershell_executable(shell) {
        return None;
    }
    try_parse_powershell_ast_commands(shell, script)
}

/// Return the semantic argv candidate for a single literal PowerShell native command.
///
/// This only classifies syntax and active native-argument mode. Callers must still prove native
/// executable resolution against their final working directory and child environment immediately
/// before spawning the command.
pub fn parse_noprofile_powershell_command_into_direct_argv(
    command: &[String],
) -> Option<PowershellDirectArgvCandidate> {
    let (shell, script) = extract_noprofile_powershell_command(command)?;
    if !is_trusted_powershell_executable(shell) {
        return None;
    }
    try_parse_powershell_ast_analysis(shell, script)?.direct_argv
}

/// A direct argv proven equivalent to a literal PowerShell native invocation in one final
/// working-directory and child-environment state.

#[derive(Debug, PartialEq, Eq)]
pub struct ProvenPowershellDirectArgv {
    command: Vec<String>,
    classified_command: Vec<String>,
    cwd: PathBuf,
    env: HashMap<String, String>,
}

impl ProvenPowershellDirectArgv {
    /// Borrow the proven canonical command for an internal policy compatibility check.
    ///
    /// The proof must still be consumed with [`Self::into_command_for_state`] before execution.
    pub fn command_for_policy(&self) -> &[String] {
        &self.command
    }

    /// Consume this proof only if the execution state is still exactly the state that was proven.
    pub fn into_command_for_state(
        self,
        classified_command: &[String],
        cwd: &Path,
        env: &HashMap<String, String>,
    ) -> Option<Vec<String>> {
        (self.classified_command == classified_command && self.cwd == cwd && self.env == *env)
            .then_some(self.command)
    }
}

/// Reclassify an exact-shape `-NoProfile -Command` invocation and prove that PowerShell and KD4's
/// direct executable lookup select the same canonical `.exe` in the supplied final execution
/// state. The returned command executes that canonical path and never performs another name lookup.
pub fn prove_noprofile_powershell_command_as_direct_argv(
    command: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Option<ProvenPowershellDirectArgv> {
    let (shell, script) = extract_noprofile_powershell_command(command)?;
    if !is_trusted_powershell_executable(shell) {
        return None;
    }

    let resolution_state = powershell_resolution_state(cwd, env)?;
    let analysis =
        try_parse_powershell_ast_analysis_with_resolution(shell, script, &resolution_state)?;
    let candidate = analysis.direct_argv?;
    let powershell_path = canonical_exe(Path::new(&analysis.resolved_application?))?;
    let direct_path = resolve_direct_exe(&candidate.argv[0], &resolution_state.path)?;
    if !same_windows_path(&powershell_path, &direct_path) {
        return None;
    }
    if candidate.native_argument_mode == "Windows"
        && uses_legacy_windows_native_arguments(&powershell_path)
    {
        return None;
    }

    let mut direct_command = Vec::with_capacity(candidate.argv.len());
    direct_command.push(powershell_path.to_str()?.to_owned());
    direct_command.extend(candidate.argv.into_iter().skip(1));
    Some(ProvenPowershellDirectArgv {
        command: direct_command,
        classified_command: command.to_vec(),
        cwd: cwd.to_path_buf(),
        env: env.clone(),
    })
}

fn powershell_resolution_state(
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Option<PowershellResolutionState> {
    Some(PowershellResolutionState {
        cwd: cwd.to_str()?.to_owned(),
        path: env_value_ignore_ascii_case(env, "PATH")?.to_owned(),
        pathext: env_value_ignore_ascii_case(env, "PATHEXT")?.to_owned(),
    })
}

fn env_value_ignore_ascii_case<'a>(
    env: &'a HashMap<String, String>,
    name: &str,
) -> Option<&'a str> {
    let mut values = env.iter().filter(|(key, _)| key.eq_ignore_ascii_case(name));
    let (_, value) = values.next()?;
    values.next().is_none().then_some(value.as_str())
}

fn resolve_direct_exe(command_name: &str, path: &str) -> Option<PathBuf> {
    let command_path = Path::new(command_name);
    if command_path.is_absolute() {
        // A rooted local executable already has a stable execution identity. UNC paths are remote
        // execution and deliberately remain in PowerShell.
        let is_local_disk = command_path.components().next().is_some_and(|component| {
            matches!(
                component,
                Component::Prefix(prefix)
                    if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
            )
        });
        return is_local_disk.then(|| canonical_exe(command_path)).flatten();
    }

    if command_name.is_empty()
        || command_name.contains(['/', '\\', ':', '*', '?', '[', ']'])
        || matches!(command_name, "." | "..")
    {
        return None;
    }

    let executable_name = match command_path.extension().and_then(OsStr::to_str) {
        None => format!("{command_name}.exe"),
        Some(extension) if extension.eq_ignore_ascii_case("exe") => command_name.to_owned(),
        Some(_) => return None,
    };

    for directory in std::env::split_paths(OsStr::new(path)) {
        // Relative and empty PATH entries depend on cwd and Windows search behavior. Do not claim
        // equivalence for them; PowerShell still handles the invocation.
        if directory.as_os_str().is_empty() || !directory.is_absolute() {
            return None;
        }
        let candidate = directory.join(&executable_name);
        if candidate.is_file() {
            return canonical_exe(&candidate);
        }
    }
    None
}

/// Resolve a native utility against this launch's cwd/environment and compare it
/// with the independently resolved host utility. Never cache this filesystem proof.
/// Script wrappers and ambiguous search state cannot establish readonly authority.
pub(crate) fn trusted_native_executable(
    executable: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Option<PathBuf> {
    let requested = Path::new(executable);
    let filename = requested.file_name()?.to_str()?;
    let host_path = std::env::var("PATH").ok()?;
    let trusted = resolve_direct_exe(filename, &host_path)?;
    native_executable_against_trusted(executable, cwd, env, &trusted)
}

fn native_executable_against_trusted(
    executable: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    trusted: &Path,
) -> Option<PathBuf> {
    let requested = Path::new(executable);
    let filename = requested.file_name()?.to_str()?;
    let path = if requested.is_absolute() {
        ""
    } else {
        env_value_ignore_ascii_case(env, "PATH")?
    };
    let resolved = resolve_direct_exe(executable, path)?;
    if !same_windows_path(trusted, &resolved) || !cwd.is_absolute() {
        return None;
    }
    if !requested.is_absolute() {
        // Native pipe and PTY launchers have different search details. Reject
        // cwd/application-directory and script-extension shadows rather than
        // assuming that PATH-only .exe resolution proves both launchers agree.
        let stem = requested.file_stem()?.to_str()?;
        let mut directories = vec![cwd.to_path_buf()];
        directories.push(std::env::current_exe().ok()?.parent()?.to_path_buf());
        for directory in directories {
            let candidate = directory.join(stem).with_extension("exe");
            if candidate.is_file()
                && canonical_exe(&candidate)
                    .is_none_or(|candidate| !same_windows_path(&candidate, trusted))
            {
                return None;
            }
        }
        let pathext = env_value_ignore_ascii_case(env, "PATHEXT")?;
        for directory in std::env::split_paths(OsStr::new(path)) {
            if directory.as_os_str().is_empty() || !directory.is_absolute() {
                return None;
            }
            let candidate = directory.join(filename);
            if candidate.is_file() {
                return canonical_exe(&candidate)
                    .filter(|candidate| same_windows_path(candidate, trusted));
            }
            if requested.extension().is_none() {
                // Mirror the PTY extension search as well as the pipe launcher's
                // .exe lookup. A script/custom extension winning either is opaque.
                for extension in pathext.split(';') {
                    let extension = extension.strip_prefix('.')?;
                    if extension.is_empty() || !extension.bytes().all(|ch| ch.is_ascii_alphanumeric()) {
                        return None;
                    }
                    let candidate = candidate.with_extension(extension);
                    if candidate.is_file() {
                        return canonical_exe(&candidate)
                            .filter(|candidate| same_windows_path(candidate, trusted));
                    }
                }
            }
        }
        return None;
    }
    Some(resolved)
}

pub fn is_trusted_native_executable_in_state(
    executable: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
) -> bool {
    trusted_native_executable(executable, cwd, env).is_some()
}

fn canonical_exe(path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    (canonical.is_file()
        && canonical
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe")))
    .then_some(canonical)
}

fn same_windows_path(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

fn uses_legacy_windows_native_arguments(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "cmd.exe" | "cscript.exe" | "find.exe" | "sqlcmd.exe" | "wscript.exe"
            )
        })
}

/// This function attempts to find a powershell.exe executable on the system.
pub fn try_find_powershell_executable_blocking() -> Option<AbsolutePathBuf> {
    try_find_powershellish_executable_in_path(&["powershell.exe"])
}

/// This function attempts to find a pwsh.exe executable on the system.
/// Note that pwsh.exe and powershell.exe are different executables:
///
/// - pwsh.exe is the cross-platform PowerShell Core (v6+) executable
/// - powershell.exe is the Windows PowerShell (v5.1 and earlier) executable
///
/// Further, while powershell.exe is included by default on Windows systems,
/// pwsh.exe must be installed separately by the user. And even when the user
/// has installed pwsh.exe, it may not be available in the system PATH, in which
/// case we attempt to locate it via other means.
pub fn try_find_pwsh_executable_blocking() -> Option<AbsolutePathBuf> {
    try_find_powershellish_executable_in_path(&["pwsh.exe"]).or_else(|| {
        let program_files = std::env::var_os("ProgramFiles")?;
        let candidate = PathBuf::from(program_files).join("PowerShell/7/pwsh.exe");
        candidate
            .is_file()
            .then(|| AbsolutePathBuf::from_absolute_path(candidate).ok())
            .flatten()
    })
}

fn try_find_powershellish_executable_in_path(candidates: &[&str]) -> Option<AbsolutePathBuf> {
    for candidate in candidates {
        let Ok(resolved_path) = which::which(candidate) else {
            continue;
        };

        let Ok(abs_path) = AbsolutePathBuf::from_absolute_path(resolved_path) else {
            continue;
        };

        return Some(abs_path);
    }

    None
}


