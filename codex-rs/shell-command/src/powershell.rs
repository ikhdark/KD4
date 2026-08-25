use std::collections::HashMap;

use std::ffi::OsStr;

use std::fs;

use std::path::Component;

use std::path::Path;
use std::path::PathBuf;

use std::path::Prefix;

use codex_utils_absolute_path::AbsolutePathBuf;

pub use crate::command_safety::PowershellDirectArgvCandidate;

use crate::command_safety::PowershellResolutionState;
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
    let script = if trimmed.starts_with(UTF8_OUTPUT_PREFIX) {
        extracted.script.to_string()
    } else {
        format!("{UTF8_OUTPUT_PREFIX}{}", extracted.script)
    };

    let mut command = command.to_vec();
    command[extracted.script_index] = script;
    command
}

struct ExtractedPowershellCommand<'a> {
    shell: &'a str,
    script: &'a str,
    script_index: usize,
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

fn extract_powershell_command_details(
    command: &[String],
) -> Option<ExtractedPowershellCommand<'_>> {
    if command.len() < 3 {
        return None;
    }

    let shell = &command[0];
    if !matches!(
        detect_shell_type(PathBuf::from(shell)),
        Some(ShellType::PowerShell)
    ) {
        return None;
    }

    let mut no_profile = false;
    let mut i = 1usize;
    while i < command.len() {
        let flag = &command[i];
        match flag.to_ascii_lowercase().as_str() {
            "-nologo" => i += 1,
            "-noprofile" => {
                no_profile = true;
                i += 1;
            }
            "-command" | "-c" => {
                let script_index = i + 1;
                if script_index + 1 != command.len() {
                    return None;
                }
                return Some(ExtractedPowershellCommand {
                    shell,
                    script: &command[script_index],
                    script_index,
                    no_profile,
                });
            }
            _ => return None,
        }
    }
    None
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
    env.iter()
        .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
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
    if let Some(ps_home) = std::process::Command::new("cmd")
        .args(["/C", "pwsh", "-NoProfile", "-Command", "$PSHOME"])
        .output()
        .ok()
        .and_then(|out| {
            if !out.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let trimmed = stdout.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
    {
        let candidate = AbsolutePathBuf::resolve_path_against_base("pwsh.exe", &ps_home);

        if is_powershellish_executable_available(candidate.as_path()) {
            return Some(candidate);
        }
    }

    try_find_powershellish_executable_in_path(&["pwsh.exe"])
}

fn try_find_powershellish_executable_in_path(candidates: &[&str]) -> Option<AbsolutePathBuf> {
    for candidate in candidates {
        let Ok(resolved_path) = which::which(candidate) else {
            continue;
        };

        if !is_powershellish_executable_available(&resolved_path) {
            continue;
        }

        let Ok(abs_path) = AbsolutePathBuf::from_absolute_path(resolved_path) else {
            continue;
        };

        return Some(abs_path);
    }

    None
}

fn is_powershellish_executable_available(powershell_or_pwsh_exe: &std::path::Path) -> bool {
    // This test works for both powershell.exe and pwsh.exe.
    std::process::Command::new(powershell_or_pwsh_exe)
        .args(["-NoLogo", "-NoProfile", "-Command", "Write-Output ok"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {

    use std::collections::HashMap;

    use std::fs;

    use super::UTF8_OUTPUT_PREFIX;
    use super::extract_powershell_command;

    use super::parse_noprofile_powershell_command_into_direct_argv;

    use super::parse_powershell_command_into_plain_commands;
    use super::prefix_powershell_script_with_utf8;

    use super::prove_noprofile_powershell_command_as_direct_argv;

    use super::resolve_direct_exe;

    use super::try_find_pwsh_executable_blocking;

    #[test]
    fn direct_resolution_accepts_rooted_local_exe_but_rejects_relative_and_unc_paths() {
        let Some(pwsh) = try_find_pwsh_executable_blocking() else {
            return;
        };
        let canonical = fs::canonicalize(pwsh.as_path()).expect("canonical pwsh");
        assert_eq!(
            resolve_direct_exe(&canonical.to_string_lossy(), ""),
            Some(canonical)
        );
        assert_eq!(resolve_direct_exe(".\\pwsh.exe", ""), None);
        assert_eq!(resolve_direct_exe("\\\\server\\share\\pwsh.exe", ""), None);
    }

    #[test]
    fn extracts_basic_powershell_command() {
        let cmd = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Write-Host hi".to_string(),
        ];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Write-Host hi");
    }

    #[test]
    fn extracts_lowercase_flags() {
        let cmd = vec![
            "powershell".to_string(),
            "-nologo".to_string(),
            "-command".to_string(),
            "Write-Host hi".to_string(),
        ];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Write-Host hi");
    }

    #[test]
    fn extracts_full_path_powershell_command() {
        let command = "C:\\windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe".to_string();
        let cmd = vec![command, "-Command".to_string(), "Write-Host hi".to_string()];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Write-Host hi");
    }

    #[test]
    fn extracts_with_noprofile_and_alias() {
        let cmd = vec![
            "pwsh".to_string(),
            "-NoProfile".to_string(),
            "-c".to_string(),
            "Get-ChildItem | Select-String foo".to_string(),
        ];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Get-ChildItem | Select-String foo");
    }

    #[test]
    fn prefixes_powershell_command_with_best_effort_utf8() {
        let cmd = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Write-Host hi".to_string(),
        ];

        let prefixed = prefix_powershell_script_with_utf8(&cmd);

        assert_eq!(
            prefixed,
            vec![
                "powershell".to_string(),
                "-Command".to_string(),
                format!("{UTF8_OUTPUT_PREFIX}Write-Host hi"),
            ]
        );
    }

    #[test]
    fn does_not_duplicate_utf8_prefix() {
        let cmd = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            format!("{UTF8_OUTPUT_PREFIX}Write-Host hi"),
        ];

        assert_eq!(prefix_powershell_script_with_utf8(&cmd), cmd);
    }

    #[test]
    fn rejects_and_does_not_rewrite_trailing_powershell_arguments() {
        let cmd = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Write-Host hi".to_string(),
            "unexpected".to_string(),
        ];

        assert_eq!(extract_powershell_command(&cmd), None);
        assert_eq!(prefix_powershell_script_with_utf8(&cmd), cmd);
    }

    #[test]
    fn parses_plain_powershell_commands() {
        let commands = parse_powershell_command_into_plain_commands(&[
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "echo hi".to_string(),
        ])
        .expect("parse");

        assert_eq!(commands, vec![vec!["echo".to_string(), "hi".to_string()]]);
    }

    #[test]
    fn parses_multiple_plain_powershell_commands() {
        let commands = parse_powershell_command_into_plain_commands(&[
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Write-Output foo | Measure-Object".to_string(),
        ])
        .expect("parse");

        assert_eq!(
            commands,
            vec![
                vec!["Write-Output".to_string(), "foo".to_string()],
                vec!["Measure-Object".to_string()],
            ]
        );
    }

    #[test]
    fn parses_command_output_assignment_and_bare_read() {
        let commands = parse_powershell_command_into_plain_commands(&[
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "$files = rg --files -g '*.rs'; $files; git status --short".to_string(),
        ])
        .expect("parse");

        assert_eq!(
            commands,
            vec![
                vec![
                    "rg".to_string(),
                    "--files".to_string(),
                    "-g".to_string(),
                    "*.rs".to_string(),
                ],
                vec![
                    "git".to_string(),
                    "status".to_string(),
                    "--short".to_string()
                ],
            ]
        );
    }

    #[test]
    fn rejects_dynamic_uses_of_command_output_assignment() {
        for script in [
            "$files = rg --files; & $files",
            "$files = rg --files; Write-Output $files",
        ] {
            assert_eq!(
                parse_powershell_command_into_plain_commands(&[
                    "powershell.exe".to_string(),
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    script.to_string(),
                ]),
                None,
                "dynamic use should remain unsupported: {script}"
            );
        }
    }

    #[test]
    fn direct_candidate_contains_semantic_native_argument_values() {
        let Some(pwsh) = try_find_pwsh_executable_blocking() else {
            return;
        };
        let command = vec![
            pwsh.as_path().to_string_lossy().into_owned(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "python \"my file.py\" \"\" \"tick``value\" \"雪\" --flag -x".to_string(),
        ];

        let candidate = parse_noprofile_powershell_command_into_direct_argv(&command)
            .expect("PowerShell 7 should classify a literal native argv");
        assert_eq!(
            candidate.argv,
            [
                "python",
                "my file.py",
                "",
                "tick`value",
                "雪",
                "--flag",
                "-x"
            ]
        );
        assert!(matches!(
            candidate.native_argument_mode.as_str(),
            "Standard" | "Windows"
        ));
        assert_eq!(candidate.powershell_version.split('.').next(), Some("7"));
    }

    #[test]
    fn direct_candidate_rejects_dynamic_and_compound_forms() {
        let Some(pwsh) = try_find_pwsh_executable_blocking() else {
            return;
        };
        for script in [
            "python $path",
            "python (Get-Location)",
            "python @args",
            "python x | Out-Null",
            "python x > out.txt",
            "python x; git status",
            "& python x",
            "$x = 'x'; python $x",
        ] {
            let command = vec![
                pwsh.as_path().to_string_lossy().into_owned(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                script.to_string(),
            ];
            assert_eq!(
                parse_noprofile_powershell_command_into_direct_argv(&command),
                None,
                "unexpected direct candidate for {script:?}"
            );
        }
    }

    #[test]
    fn final_state_proof_executes_canonical_exe_and_is_state_bound() {
        let Some(pwsh) = try_find_pwsh_executable_blocking() else {
            return;
        };
        let executable = fs::canonicalize(pwsh.as_path()).expect("canonical pwsh");
        let executable_dir = executable.parent().expect("pwsh parent");
        let cwd = std::env::current_dir().expect("cwd");
        let mut env = HashMap::from([
            ("Path".to_string(), executable_dir.display().to_string()),
            ("Pathext".to_string(), ".COM;.EXE;.BAT;.CMD".to_string()),
        ]);
        let command = vec![
            pwsh.as_path().to_string_lossy().into_owned(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "pwsh --version".to_string(),
        ];

        let proof = prove_noprofile_powershell_command_as_direct_argv(&command, &cwd, &env)
            .expect("matching final-state resolvers");
        let direct = proof
            .into_command_for_state(&command, &cwd, &env)
            .expect("unchanged state");
        assert!(super::same_windows_path(
            std::path::Path::new(&direct[0]),
            &executable
        ));
        assert_eq!(&direct[1..], ["--version"]);

        let proof = prove_noprofile_powershell_command_as_direct_argv(&command, &cwd, &env)
            .expect("matching final-state resolvers");
        let rewritten_cwd = cwd.join("hook-rewritten-cwd");
        assert_eq!(
            proof.into_command_for_state(&command, &rewritten_cwd, &env),
            None
        );

        let proof = prove_noprofile_powershell_command_as_direct_argv(&command, &cwd, &env)
            .expect("matching final-state resolvers");
        env.insert("Path".to_string(), cwd.display().to_string());
        assert_eq!(proof.into_command_for_state(&command, &cwd, &env), None);

        let mut env = HashMap::from([
            ("Path".to_string(), executable_dir.display().to_string()),
            ("Pathext".to_string(), ".COM;.EXE;.BAT;.CMD".to_string()),
        ]);
        let proof = prove_noprofile_powershell_command_as_direct_argv(&command, &cwd, &env)
            .expect("matching final-state resolvers");
        env.insert("HOOK_CHANGED_ENV".to_string(), "1".to_string());
        assert_eq!(proof.into_command_for_state(&command, &cwd, &env), None);

        let env = HashMap::from([
            ("Path".to_string(), executable_dir.display().to_string()),
            ("Pathext".to_string(), ".COM;.EXE;.BAT;.CMD".to_string()),
        ]);
        let proof = prove_noprofile_powershell_command_as_direct_argv(&command, &cwd, &env)
            .expect("matching final-state resolvers");
        let mut rewritten_command = command;
        rewritten_command[3] = "pwsh -Help".to_string();
        assert_eq!(
            proof.into_command_for_state(&rewritten_command, &cwd, &env),
            None
        );
    }

    #[test]
    fn path_shadowing_and_powershell_commands_fail_closed() {
        let Some(pwsh) = try_find_pwsh_executable_blocking() else {
            return;
        };
        let executable = fs::canonicalize(pwsh.as_path()).expect("canonical pwsh");
        let executable_dir = executable.parent().expect("pwsh parent");
        let cwd = std::env::current_dir().expect("cwd");
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let shadow_dir = std::env::temp_dir().join(format!(
            "codex-powershell-shadow-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&shadow_dir).expect("create shadow dir");
        fs::write(shadow_dir.join("pwsh.cmd"), "@echo off\r\n").expect("write shim");
        let path = std::env::join_paths([shadow_dir.as_path(), executable_dir])
            .expect("join PATH")
            .to_string_lossy()
            .into_owned();
        let env = HashMap::from([
            ("PATH".to_string(), path),
            ("PATHEXT".to_string(), ".COM;.EXE;.BAT;.CMD".to_string()),
        ]);
        let native = vec![
            pwsh.as_path().to_string_lossy().into_owned(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "pwsh --version".to_string(),
        ];
        assert_eq!(
            prove_noprofile_powershell_command_as_direct_argv(&native, &cwd, &env),
            None
        );

        let cmdlet = vec![
            pwsh.as_path().to_string_lossy().into_owned(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Get-ChildItem value".to_string(),
        ];
        assert_eq!(
            prove_noprofile_powershell_command_as_direct_argv(&cmdlet, &cwd, &env),
            None
        );
        fs::remove_dir_all(&shadow_dir).expect("remove shadow dir");
    }
}
