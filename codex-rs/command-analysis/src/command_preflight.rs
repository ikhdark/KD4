use std::borrow::Cow;
use std::path::Path;

#[path = "full_suite.rs"]
mod full_suite;
pub use full_suite::FullSuiteBudget;
pub use full_suite::FullSuitePermit;

#[path = "managed_cargo_lane.rs"]
mod managed_cargo_lane;
pub use managed_cargo_lane::check_managed_cargo_lane;
pub use managed_cargo_lane::compilation_sources;

use super::command_search::rg_search_path_indices;
use super::command_search::rg_search_path_operands;
use crate::command_shape::CommandInvocation;
use codex_shell_command::shell_detect::ShellType;


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandPreflightIssueCode {
    DirectArgvPowerShellCmdlet,
    KnownFlagTypo,
    RgLiteralGlobPath,
    PowerShellSyntax,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPreflightRetry {
    Argv { program: String, args: Vec<String> },
    PowerShellScript { script_body: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPreflightRejected {
    Argv(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandPreflightIssue {
    code: CommandPreflightIssueCode,
    rejected: CommandPreflightRejected,
    detail: String,
    guidance: Option<String>,
    retry: Option<CommandPreflightRetry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPreflightOutcome {
    pub invocation: CommandInvocation,
    pub validation_invocations: Vec<CommandInvocation>,
    pub repair_notice: Option<String>,
    /// Explains a likely mistake in a script that still runs unchanged. It
    /// never admits the command as repaired.
    pub advisory: Option<String>,
}

impl CommandPreflightOutcome {
    pub fn repaired(&self) -> bool {
        self.repair_notice.is_some()
    }

    pub fn model_notice(&self) -> Option<String> {
        match (&self.repair_notice, &self.advisory) {
            (Some(repair), Some(advisory)) => Some(format!("{repair}\n{advisory}")),
            (repair, advisory) => repair.clone().or_else(|| advisory.clone()),
        }
    }
}

impl CommandPreflightIssue {
    fn reject(
        code: CommandPreflightIssueCode,
        rejected: CommandPreflightRejected,
        detail: String,
        guidance: Option<String>,
        retry: Option<CommandPreflightRetry>,
    ) -> Self {
        Self {
            code,
            rejected,
            detail,
            guidance,
            retry,
        }
    }

    pub fn render_for_model(&self) -> String {
        let mut rendered = format!(
            "Command rejected: `{}`\nReason: {}",
            self.rejected.render_for_model(),
            self.detail
        );
        match &self.retry {
            Some(retry) => {
                rendered.push_str("\nUse: ");
                rendered.push_str(&retry.render_for_model());
                rendered.push('.');
            }
            None => {
                if let Some(guidance) = &self.guidance {
                    rendered.push_str("\nUse: ");
                    rendered.push_str(guidance);
                }
            }
        }
        let metadata = serde_json::json!({
            "kind": self.code.tool_error_kind(),
            "summary": self.detail,
        });
        rendered.push_str("\nTool error metadata: ");
        rendered.push_str(&metadata.to_string());
        rendered
    }

    fn render_advisory_for_model(&self) -> String {
        let mut rendered = format!(
            "Command preflight advisory ({}): {}",
            self.code.tool_error_kind(),
            self.detail
        );
        if let Some(guidance) = &self.guidance {
            rendered.push_str(" To search it, ");
            rendered.push_str(guidance);
        }
        rendered.push_str(" The script is unchanged. Inspect this search's exit status and stderr; a path error is not evidence of no matches. Preserve successful results from other commands and correct only the failed search.");
        rendered
    }
}

impl CommandPreflightIssueCode {
    fn tool_error_kind(self) -> &'static str {
        match self {
            Self::DirectArgvPowerShellCmdlet => "direct_argv_powershell_cmdlet",
            Self::KnownFlagTypo => "known_flag_typo",
            Self::RgLiteralGlobPath => "rg_literal_glob_path",
            Self::PowerShellSyntax => "powershell_syntax",
        }
    }
}

impl CommandPreflightRejected {
    fn render_for_model(&self) -> String {
        match self {
            Self::Argv(argv) => truncate(&codex_shell_command::parse_command::shlex_join(argv)),
        }
    }
}

impl CommandPreflightRetry {
    fn render_for_model(&self) -> String {
        match self {
            Self::Argv { program, args } => format!(
                "program: {}, args: [{}]",
                json_string(program),
                args.iter()
                    .map(|arg| json_string(arg))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::PowerShellScript { script_body } => {
                format!("script_body: {}", json_string(script_body))
            }
        }
    }
}


fn preflight_command_issue(
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<Vec<Vec<String>>, CommandPreflightIssue> {
    preflight_command_issues(command, shell_type, /*literal_glob_advisories*/ None)
}

/// With `literal_glob_advisories`, a literal `rg` glob path is collected
/// instead of rejecting the command. A script's other commands still produce
/// useful output, and `rg` reports the missing path itself.
fn preflight_command_issues(
    command: &[String],
    shell_type: Option<ShellType>,
    mut literal_glob_advisories: Option<&mut Vec<CommandPreflightIssue>>,
) -> Result<Vec<Vec<String>>, CommandPreflightIssue> {
    let preflight_shell_type = shell_type.or_else(|| infer_direct_shell_type(command));
    if preflight_shell_type == Some(ShellType::PowerShell)
        && let Some(diagnostic) = codex_shell_command::powershell_command_syntax_error(command)
    {
        return Err(CommandPreflightIssue::reject(
            CommandPreflightIssueCode::PowerShellSyntax,
            CommandPreflightRejected::Argv(command.to_vec()),
            format!(
                "PowerShell reported invalid script syntax; no command was started.\n{diagnostic}"
            ),
            Some(
                "Correct the script syntax. Changing to script_body cannot repair invalid source."
                    .into(),
            ),
            None,
        ));
    }
    // Parsing here extracts validation metadata; the target shell owns script
    // syntax and expansion. Heuristic quote/name/path checks can reject valid scripts.
    let argv_commands = argv_commands(command, preflight_shell_type).unwrap_or_default();

    for argv in &argv_commands {
        // A basename cannot prove that a native executable does not exist.
        if let Some(advisories) = literal_glob_advisories.as_deref_mut()
            && let Err(issue) = lint_direct_argv_powershell_cmdlet(argv, preflight_shell_type)
        {
            advisories.push(issue);
        }
        lint_known_flag_typos(argv)?;
        if let Err(issue) = lint_rg_literal_glob_paths(argv, preflight_shell_type) {
            match literal_glob_advisories.as_deref_mut() {
                Some(advisories) => advisories.push(issue),
                None => return Err(issue),
            }
        }
    }

    Ok(argv_commands)
}


pub async fn preflight_invocation_with_equivalent_repair_async(
    invocation: &CommandInvocation,
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<CommandPreflightOutcome, String> {
    let invocation = invocation.clone();
    let command = command.to_vec();
    crate::run_blocking_command_analysis(move || {
        preflight_invocation_with_equivalent_repair_detailed(&invocation, &command, shell_type)
            .map_err(|issue| issue.render_for_model())
    })
    .await
    .map_err(|error| format!("command preflight worker failed: {error}"))?
}

pub async fn preflight_invocation_for_kd4_runtime(
    kd4_runtime: bool,
    direct_runtime: bool,
    invocation: &CommandInvocation,
    command: &[String],
    shell_type: Option<ShellType>,
    local_cwd: Option<&Path>,
) -> Result<CommandPreflightOutcome, String> {
    if !kd4_runtime {
        return Ok(CommandPreflightOutcome {
            invocation: invocation.clone(),
            validation_invocations: vec![invocation.clone()],
            repair_notice: None,
            advisory: None,
        });
    }
    let mut outcome =
        match preflight_invocation_for_runtime(direct_runtime, invocation, command, shell_type)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                // Windows cannot name a literal '*' file. Expand only a bounded,
                // single-directory, read-only argv search; scripts retain their
                // advisory so pipelines, expansion and side effects are untouched.
                let repaired = if cfg!(windows)
                    && !direct_runtime
                    && let Some(cwd) = local_cwd
                    && invocation.is_argv()
                {
                    let invocation = invocation.clone();
                    let cwd = cwd.to_path_buf();
                    crate::run_blocking_command_analysis(move || {
                        repair_rg_leaf_glob(&invocation, &cwd)
                    })
                    .await
                    .ok()
                    .flatten()
                } else {
                    None
                };
                repaired.ok_or(error)?
            }
        };
    // Never inspect the host filesystem for a remote command. Direct-runtime
    // bypass remains authoritative, including for optional advisories.
    if !direct_runtime && let Some(cwd) = local_cwd {
        if cfg!(windows)
            && !outcome.repaired()
            && outcome
                .advisory
                .as_deref()
                .is_some_and(|notice| notice.contains("rg_literal_glob_path"))
            && shell_type.or_else(|| infer_direct_shell_type(command))
                == Some(ShellType::PowerShell)
        {
            let invocation = invocation.clone();
            let command = command.to_vec();
            let cwd = cwd.to_path_buf();
            if let Ok(Some(repaired)) = crate::run_blocking_command_analysis(move || {
                repair_powershell_rg_leaf_glob(&invocation, &command, &cwd)
            })
            .await
            {
                outcome = repaired;
            }
        }
        // The parser's cached literal stages are advice only. Validation keeps
        // the complete invocation; neither path adds filesystem probes. Without
        // an active syntax scope the lowering is a blocking parser round trip,
        // so it runs on the analysis worker like every other parse here.
        let advisory_shell = shell_type.or_else(|| infer_direct_shell_type(command));
        let advisory_command = command.to_vec();
        if advisory_shell == Some(ShellType::PowerShell)
            && let Ok(Some(notice)) = crate::run_blocking_command_analysis(move || {
                source_paging_advisory(&advisory_command, advisory_shell)
            })
            .await
        {
            outcome.advisory = Some(match outcome.advisory {
                Some(existing) => format!("{existing}\n{notice}"),
                None => notice.to_string(),
            });
        }

    }
    Ok(outcome)
}

fn repair_powershell_rg_leaf_glob(
    invocation: &CommandInvocation,
    command: &[String],
    cwd: &Path,
) -> Option<CommandPreflightOutcome> {
    if invocation.is_argv() {
        return None;
    }
    // The existing AST classifier excludes pipelines, redirections, variables,
    // compound statements and profile-dependent interpretation. Keep the shell
    // instead of claiming native executable resolution is proven here.
    let candidate =
        codex_shell_command::powershell::parse_noprofile_powershell_command_into_direct_argv(
            command,
        )?;
    let (program, args) = candidate.argv.split_first()?;
    let repaired = repair_rg_leaf_glob(
        &CommandInvocation::Argv {
            program: program.clone(),
            args: args.to_vec(),
        },
        cwd,
    )?;
    let argv = repaired.invocation.to_direct_argv()?;
    let script = format!(
        "& {}",
        argv.iter()
            .map(|arg| format!("'{}'", arg.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let rewritten = match invocation {
        CommandInvocation::Script(_) => CommandInvocation::Script(script),
        CommandInvocation::PowerShellScript(_) => CommandInvocation::PowerShellScript(script),
        CommandInvocation::Argv { .. } => return None,
    };
    Some(CommandPreflightOutcome {
        validation_invocations: repaired.validation_invocations,
        repair_notice: Some(read_only_repair_notice(
            CommandPreflightIssueCode::RgLiteralGlobPath,
            invocation,
            &rewritten,
        )),
        invocation: rewritten,
        advisory: None,
    })
}

fn repair_rg_leaf_glob(
    invocation: &CommandInvocation,
    cwd: &Path,
) -> Option<CommandPreflightOutcome> {
    let argv = invocation.to_direct_argv()?;
    if !argv.first().is_some_and(|program| is_rg_program(program))
        || !codex_shell_command::is_safe_command::is_known_safe_direct_argv(&argv)
    {
        return None;
    }
    let indices = rg_search_path_indices(&argv)?;
    // Expand one leaf glob in place, not a global --glob that would narrow
    // other roots. The shared argument parser keeps patterns/flag values out.
    let globs = indices
        .iter()
        .copied()
        .filter(|&index| argv[index].contains('*'))
        .collect::<Vec<_>>();
    let [operand_index] = globs.as_slice() else {
        return None;
    };
    let operand = &argv[*operand_index];
    let root = cwd.canonicalize().ok()?;
    for &index in &indices {
        if index == *operand_index {
            continue;
        }
        // Preserve uncertainty rather than guessing another missing operand.
        let path = cwd.join(&argv[index]).canonicalize().ok()?;
        if !path.starts_with(&root) {
            return None;
        }
    }
    let path = Path::new(operand);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let leaf = path.file_name()?.to_str()?;
    if parent.to_str()?.contains(['*', '?', '[', ']'])
        || leaf.contains(['?', '[', ']', '{', '}'])
        || leaf.contains("**")
    {
        return None;
    }
    let pattern = glob::Pattern::new(leaf).ok()?;
    let directory = cwd.join(parent).canonicalize().ok()?;
    if !directory.starts_with(&root) {
        return None;
    }
    let mut matches = Vec::new();
    for (index, entry) in std::fs::read_dir(directory).ok()?.enumerate() {
        if index >= 4096 {
            return None;
        }
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_str()?;
        if pattern.matches(name) {
            // Do not introduce recursion or dereference links as a repair.
            if !entry.file_type().ok()?.is_file() || matches.len() >= 256 {
                return None;
            }
            matches.push(parent.join(name).to_str()?.to_string());
        }
    }
    if matches.is_empty() {
        return None;
    }
    matches.sort();
    let mut repaired_argv = argv[..*operand_index].to_vec();
    // Prevent a matched '-name' from becoming an option, including a helper.
    if *operand_index == argv.len() - 1 && !repaired_argv.iter().any(|arg| arg == "--") {
        repaired_argv.push("--".to_string());
    }
    repaired_argv.extend(matches);
    repaired_argv.extend_from_slice(&argv[*operand_index + 1..]);
    if !codex_shell_command::is_safe_command::is_known_safe_direct_argv(&repaired_argv) {
        return None;
    }
    // Do not chain a typo fix with this expansion.
    let stages = preflight_command_issue(&repaired_argv, None).ok()?;
    let repaired = CommandInvocation::Argv {
        program: repaired_argv[0].clone(),
        args: repaired_argv[1..].to_vec(),
    };
    Some(CommandPreflightOutcome {
        validation_invocations: validation_invocations(stages, &repaired),
        repair_notice: Some(read_only_repair_notice(
            CommandPreflightIssueCode::RgLiteralGlobPath,
            invocation,
            &repaired,
        )),
        invocation: repaired,
        advisory: None,
    })
}

fn source_paging_advisory(
    command: &[String],
    shell_type: Option<ShellType>,
) -> Option<&'static str> {
    if shell_type != Some(ShellType::PowerShell) {
        return None;
    }
    let mut reads = 0;
    let mut paged = false;
    for argv in argv_commands(command, shell_type)? {
        let (program, args) = argv.split_first()?;
        if program.eq_ignore_ascii_case("Get-Content") {
            // Encoding conversions, byte streams and live tails are not native
            // UTF-8 snapshot reads. Do not suggest replacing their semantics.
            if args.iter().any(|arg| {
                arg.starts_with('-')
                    && !["-LiteralPath", "-Path", "-TotalCount", "-Head"]
                        .iter()
                        .any(|allowed| arg.eq_ignore_ascii_case(allowed))
            }) {
                return None;
            }
            reads += 1;
            paged |= args.windows(2).any(|pair| {
                matches_ignore_ascii_case(&pair[0], &["-TotalCount", "-Head"])
                    && pair[1].parse::<usize>().is_ok()
            });
        } else if program.eq_ignore_ascii_case("Select-Object") {
            if args.is_empty()
                || args.len() % 2 != 0
                || !args.as_chunks::<2>().0.iter().all(|pair| {
                    matches_ignore_ascii_case(&pair[0], &["-Skip", "-First"])
                        && pair[1].parse::<usize>().is_ok()
                })
            {
                return None;
            }
            paged = true;
        } else {
            // Mixed scripts may transform data or mutate state. Leave them alone.
            return None;
        }
    }
    (reads > 0 && paged).then_some(
        "Source-read advisory: this paged shell read runs unchanged. For subsequent UTF-8 source inspection, prefer read_file with batched line selectors (1-based, inclusive); PowerShell -Skip N starts at line N+1. Check every result and file_complete; recover omitted bytes from the returned snapshot instead of rereading pages. Keep shell reads when their encoding, streaming, or transformation semantics are required. Do not reread already delivered evidence solely to change tools.",
    )
}

pub async fn preflight_invocation_for_runtime(
    direct_runtime: bool,
    invocation: &CommandInvocation,
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<CommandPreflightOutcome, String> {
    if direct_runtime {
        return Ok(CommandPreflightOutcome {
            invocation: invocation.clone(),
            validation_invocations: Vec::new(),
            repair_notice: None,
            advisory: None,
        });
    }
    preflight_invocation_with_equivalent_repair_async(invocation, command, shell_type).await
}

fn preflight_invocation_with_equivalent_repair_detailed(
    invocation: &CommandInvocation,
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<CommandPreflightOutcome, CommandPreflightIssue> {
    // Direct argv is a single command, so rejecting it costs nothing. A script
    // runs its other commands too; discarding all of them for one literal glob
    // path forces the whole script to be regenerated.
    let mut literal_glob_advisories = Vec::new();
    let mut issue = match preflight_command_issues(
        command,
        shell_type,
        (!invocation.is_argv()).then_some(&mut literal_glob_advisories),
    ) {
        Ok(argv_commands) => {
            if let Some(repaired) = git_status_read_only_equivalent(invocation) {
                let Some(repaired_command) = repaired.to_direct_argv() else {
                    return Ok(CommandPreflightOutcome {
                        invocation: invocation.clone(),
                        validation_invocations: validation_invocations(argv_commands, invocation),
                        repair_notice: None,
                        advisory: None,
                    });
                };
                let repaired_argv_commands =
                    preflight_command_issue(&repaired_command, /*shell_type*/ None)?;
                return Ok(CommandPreflightOutcome {
                    // Status is already valid. Disabling optional locks and
                    // fsmonitor is normalization, not a retry-admission bypass.
                    repair_notice: None,
                    advisory: None,
                    validation_invocations: validation_invocations(
                        repaired_argv_commands,
                        &repaired,
                    ),
                    invocation: repaired,
                });
            }
            let advisory = (!literal_glob_advisories.is_empty()).then(|| {
                literal_glob_advisories
                    .iter()
                    .map(CommandPreflightIssue::render_advisory_for_model)
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            return Ok(CommandPreflightOutcome {
                invocation: invocation.clone(),
                validation_invocations: validation_invocations(argv_commands, invocation),
                repair_notice: None,
                advisory,
            });
        }
        Err(issue) => issue,
    };

    // A repair is execution-safe only for direct argv. Rewriting a script could
    // discard pipelines, redirection, variable expansion, or a later mutating
    // command even when the first parsed argv happens to look read-only.
    let exact_direct_argv = invocation.to_direct_argv();
    if !matches!(&issue.rejected, CommandPreflightRejected::Argv(argv)
        if exact_direct_argv.as_ref() == Some(argv) && argv.as_slice() == command
            && shell_type.or_else(|| infer_direct_shell_type(command)).is_none())
    {
        // The leaf is not a retry for its wrapper, streams or other stages.
        // Issues without a leaf retry (script syntax, glob paths) keep their
        // own remediation.
        if issue.retry.take().is_some() {
            issue.guidance = Some(
                "Correct the flag in the original invocation, preserving its shell and all stages."
                    .into(),
            );
        }
        return Err(issue);
    }

    // Keep shell conversion as retry advice: the caller's shell, environment,
    // and login settings are not bound by this direct-argv preflight.
    let Some(CommandPreflightRetry::Argv { program, args }) = issue.retry.as_ref() else {
        return Err(issue);
    };
    let program_name = program_name(program);
    let read_only_equivalent = match issue.code {
        CommandPreflightIssueCode::KnownFlagTypo => {
            matches_ignore_ascii_case(program_name, &["rg", "rga", "grep"])
        }
        _ => false,
    };
    if !read_only_equivalent {
        return Err(issue);
    }

    let repaired = CommandInvocation::Argv {
        program: program.clone(),
        args: args.clone(),
    };
    let Some(repaired_command) = repaired.to_direct_argv() else {
        return Err(issue);
    };
    // Search executables can also launch helpers (for example rg --pre).
    // Verify the complete repaired argv before automatically executing it.
    if !codex_shell_command::is_safe_command::is_known_safe_direct_argv(&repaired_command) {
        return Err(issue);
    }
    // One repair is the hard limit. If the repaired command has another issue,
    // reject it rather than chaining mechanical transformations.
    let repaired_argv_commands =
        preflight_command_issue(&repaired_command, /*shell_type*/ None)?;

    let repair_notice = read_only_repair_notice(issue.code, invocation, &repaired);
    Ok(CommandPreflightOutcome {
        validation_invocations: validation_invocations(repaired_argv_commands, &repaired),
        invocation: repaired,
        repair_notice: Some(repair_notice),
        advisory: None,
    })
}

fn validation_invocations(
    _argv_commands: Vec<Vec<String>>,
    fallback: &CommandInvocation,
) -> Vec<CommandInvocation> {
    // Both handlers consume this for authority, not display. Preserve source
    // operators, expansions, host modes and exit guards with their leaves.
    vec![fallback.clone()]
}

fn git_status_read_only_equivalent(invocation: &CommandInvocation) -> Option<CommandInvocation> {
    let CommandInvocation::Argv { program, args } = invocation else {
        return None;
    };
    let command = invocation.to_direct_argv()?;
    let (subcommand_index, _) =
        codex_shell_command::is_dangerous_command::find_git_subcommand(&command, &["status"])?;
    let global_args = &command[1..subcommand_index];
    let locks_disabled = global_args
        .iter()
        .any(|arg| arg == "--no-optional-locks");
    let fsmonitor_disabled = global_args.iter().any(|arg| arg == "-ccore.fsmonitor=false")
        || global_args
            .windows(2)
            .any(|args| args[0] == "-c" && args[1] == "core.fsmonitor=false");
    if locks_disabled && fsmonitor_disabled {
        return None;
    }

    let mut repaired_args = Vec::with_capacity(args.len() + 3);
    if !locks_disabled {
        repaired_args.push("--no-optional-locks".to_string());
    }
    if !fsmonitor_disabled {
        repaired_args.extend(["-c".to_string(), "core.fsmonitor=false".to_string()]);
    }
    repaired_args.extend(args.iter().cloned());
    Some(CommandInvocation::Argv {
        program: program.clone(),
        args: repaired_args,
    })
}

fn read_only_repair_notice(
    code: CommandPreflightIssueCode,
    original: &CommandInvocation,
    repaired: &CommandInvocation,
) -> String {
    format!(
        "Command preflight applied one read-only equivalent repair ({}) before execution.\nOriginal: {}\nExecuted: {}",
        code.tool_error_kind(),
        original.display_command(),
        repaired.display_command()
    )
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
}

fn extract_cmd_command(command: &[String]) -> Option<Cow<'_, str>> {
    for (index, arg) in command.iter().skip(1).enumerate() {
        let trimmed = arg.trim();
        if trimmed.eq_ignore_ascii_case("/c") || trimmed.eq_ignore_ascii_case("/k") {
            return command_tail(command, index + 2);
        }
        if let Some(script) = cmd_switch_inline_script(trimmed) {
            // Inline /c and /k start the same command-line tail as a separate
            // switch. Later launch arguments remain part of that source.
            return Some(match command_tail(command, index + 2) {
                Some(tail) => Cow::Owned(format!("{script} {tail}")),
                None => Cow::Borrowed(script),
            });
        }
    }
    None
}

fn cmd_switch_inline_script(arg: &str) -> Option<&str> {
    ["/c", "/k"].iter().find_map(|switch| {
        arg.get(..switch.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(switch))
            .then(|| arg[switch.len()..].trim_start())
            .filter(|script| !script.is_empty())
    })
}

fn command_tail(command: &[String], start: usize) -> Option<Cow<'_, str>> {
    match command.get(start..) {
        Some([]) | None => None,
        Some([script]) => Some(Cow::Borrowed(script.as_str())),
        Some(args) => Some(Cow::Owned(args.join(" "))),
    }
}

pub fn infer_direct_shell_type(command: &[String]) -> Option<ShellType> {
    let program = command.first().map(|program| program_name(program))?;
    if program.eq_ignore_ascii_case("pwsh") || program.eq_ignore_ascii_case("powershell") {
        Some(ShellType::PowerShell)
    } else if program.eq_ignore_ascii_case("cmd") || program.eq_ignore_ascii_case("cmd.exe") {
        Some(ShellType::Cmd)
    } else if program.eq_ignore_ascii_case("bash") {
        Some(ShellType::Bash)
    } else if program.eq_ignore_ascii_case("zsh") {
        Some(ShellType::Zsh)
    } else if program.eq_ignore_ascii_case("sh") {
        Some(ShellType::Sh)
    } else {
        None
    }
}

fn argv_commands(command: &[String], shell_type: Option<ShellType>) -> Option<Vec<Vec<String>>> {
    match shell_type {
        Some(ShellType::Bash | ShellType::Zsh | ShellType::Sh) => {
            codex_shell_command::bash::parse_shell_lc_plain_commands(command)
        }
        Some(ShellType::PowerShell) => {
            codex_shell_command::powershell::parse_powershell_command_into_plain_commands(command)
        }
        Some(ShellType::Cmd) => parse_cmd_plain_commands(command),
        None => {
            if command.is_empty() {
                Some(Vec::new())
            } else {
                Some(vec![command.to_vec()])
            }
        }
    }
}

pub fn rg_argv_commands(
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<Vec<Vec<String>>, String> {
    match argv_commands(command, shell_type) {
        Some(commands) => Ok(commands),
        None if command_may_invoke_rg(command) => Err(
            "`rg` search rejected because the shell command could not be parsed precisely enough to verify its search scope. Use direct argv or a statically parseable shell command."
                .to_string(),
        ),
        None => Ok(Vec::new()),
    }
}

fn command_may_invoke_rg(command: &[String]) -> bool {
    command.iter().any(|argument| {
        argument
            .split(|ch: char| {
                ch.is_whitespace()
                    || matches!(
                        ch,
                        '|' | '&' | ';' | '(' | ')' | '{' | '}' | '<' | '>' | '"' | '\''
                    )
            })
            .map(|token| token.trim_matches([',', '`']))
            .any(is_rg_program)
    })
}

fn parse_cmd_plain_commands(command: &[String]) -> Option<Vec<Vec<String>>> {
    let script = extract_cmd_command(command)?;
    split_cmd_plain_commands(script.as_ref())
}

/// Conservative PowerShell source ownership for admission, not argv or exit
/// proof. Quoted data stays whole; only active separators and subexpressions
/// introduce possible commands. No shell process is launched.
fn possible_powershell_commands(script: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut subexpressions = Vec::new();
    let mut chars = script.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == char::from(96) && quote != Some('\'') {
            if let Some(escaped) = chars.next() {
                word.push(escaped);
            }
            continue;
        }
        if ch == '<' && quote.is_none() && chars.peek() == Some(&'#') {
            chars.next();
            while let Some(ch) = chars.next() {
                if ch == '#' && chars.peek() == Some(&'>') {
                    chars.next();
                    break;
                }
            }
            continue;
        }
        if ch == '#' && quote.is_none() && word.is_empty() {
            for ch in chars.by_ref() {
                if ch == '\n' {
                    break;
                }
            }
            if !words.is_empty() {
                commands.push(std::mem::take(&mut words));
            }
            continue;
        }
        if ch == '$' && quote != Some('\'') && chars.peek() == Some(&'(') {
            chars.next();
            subexpressions.push(quote.take());
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            if !words.is_empty() {
                commands.push(std::mem::take(&mut words));
            }
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                if ch == '\'' && chars.peek() == Some(&'\'') {
                    chars.next();
                    word.push('\'');
                } else {
                    quote = None;
                }
            } else if quote.is_none() {
                quote = Some(ch);
            } else {
                word.push(ch);
            }
            continue;
        }
        if quote.is_some() {
            word.push(ch);
            continue;
        }
        if matches!(ch, ';' | '|' | '&' | '(' | ')' | '{' | '}' | '\r' | '\n') {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            if !words.is_empty() {
                commands.push(std::mem::take(&mut words));
            }
            if ch == ')'
                && let Some(previous) = subexpressions.pop()
            {
                quote = previous;
            }
        } else if ch.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    if !words.is_empty() {
        commands.push(words);
    }
    for argv in &mut commands {
        if argv.first().is_some_and(|word| word.starts_with('$')) {
            if let Some(index) = argv.iter().position(|word| word == "=") {
                argv.drain(..=index);
            } else if let Some((_, value)) = argv[0].split_once('=') {
                argv[0] = value.to_owned();
            }
        }
    }
    commands
}

fn split_cmd_plain_commands(script: &str) -> Option<Vec<Vec<String>>> {
    split_cmd_commands(script, true)
}

/// Keep CMD command boundaries for possible-build admission even when shell
/// state cannot be lowered precisely. This is not an argv or environment proof.
fn possible_cmd_commands(script: &str) -> Option<Vec<Vec<String>>> {
    split_cmd_commands(script, false)
}

fn split_cmd_commands(script: &str, require_plain: bool) -> Option<Vec<Vec<String>>> {
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = script.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            // Expansion, blocks, CALL and non-plain builtins need a native
            // grammar/state proof, not literal path or lint authority.
            '%' | '!' | '(' | ')' if require_plain => return None,
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '^' if !quoted => {
                let escaped = chars.next()?;
                if matches!(escaped, '\r' | '\n') {
                    if require_plain {
                        return None;
                    }
                    if escaped == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    continue;
                }
                word.push(escaped);
                started = true;
            }
            '<' | '>' if !quoted && require_plain => return None,
            '<' | '>' if !quoted => {
                if started {
                    if word.chars().all(|ch| ch.is_ascii_digit()) {
                        word.clear();
                    } else {
                        words.push(std::mem::take(&mut word));
                    }
                    started = false;
                }
                if chars.peek() == Some(&ch) {
                    chars.next();
                }
                skip_cmd_redirection_target(&mut chars)?;
            }
            '&' | '|' | '(' | ')' | '\r' | '\n' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
                if chars.peek() == Some(&ch) {
                    chars.next();
                }
            }
            ch if ch.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(ch);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        words.push(word);
    }
    if !words.is_empty() {
        commands.push(words);
    }
    if require_plain
        && commands.iter().any(|argv| {
            argv.first().is_some_and(|program| {
                matches!(
                    program.to_ascii_lowercase().as_str(),
                    "call" | "for" | "if" | "rem" | "set" | "cmd"
                ) || program.starts_with('@')
            })
        })
    {
        return None;
    }
    if !require_plain {
        for argv in &mut commands {
            if let Some(program) = argv.first_mut() {
                *program = program.trim_start_matches('@').to_string();
            }
        }
    }
    Some(commands)
}

fn skip_cmd_redirection_target(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<()> {
    while chars.peek().is_some_and(|ch| matches!(*ch, ' ' | '\t')) {
        chars.next();
    }
    // Descriptor duplication, such as `2>&1`, is not a command separator.
    if chars.peek() == Some(&'&') {
        chars.next();
    }
    let mut started = false;
    let mut quoted = false;
    while let Some(&ch) = chars.peek() {
        match ch {
            '"' => quoted = !quoted,
            '^' if !quoted => {
                chars.next();
                chars.next()?;
                started = true;
                continue;
            }
            '&' | '|' | '(' | ')' | '<' | '>' | '\r' | '\n' if !quoted => break,
            ch if ch.is_whitespace() && !quoted => break,
            _ => {}
        }
        chars.next();
        started = true;
    }
    (started && !quoted).then_some(())
}

fn lint_known_flag_typos(argv: &[String]) -> Result<(), CommandPreflightIssue> {
    let Some(program) = argv.first().map(|program| program_name(program)) else {
        return Ok(());
    };
    let mut index = 1;
    while let Some(arg) = argv.get(index) {
        if arg == "--" {
            break;
        }
        if !arg.starts_with('-') {
            index += 1;
            continue;
        }
        if let Some((bad, good)) = known_flag_fix(program, arg) {
            let suggested = suggested_argv(argv, index, good);
            return Err(CommandPreflightIssue::reject(
                CommandPreflightIssueCode::KnownFlagTypo,
                CommandPreflightRejected::Argv(argv.to_vec()),
                format!("`{program}` has no `{bad}` flag."),
                None,
                retry_argv_from_command(&suggested),
            ));
        }
        // Only continue through options whose arity is established. In
        // particular, a dash-prefixed regexp is data after -e/--regexp.
        if matches_ignore_ascii_case(program, &["rg", "rga", "grep"]) {
            if rg_option_consumes_next(arg) {
                index += 2;
                continue;
            }
            if !arg.starts_with("--") && arg.len() > 2 {
                let mut consumes_next = false;
                for (offset, flag) in arg[1..].char_indices() {
                    if rg_option_consumes_next(&format!("-{flag}")) {
                        consumes_next = offset + flag.len_utf8() == arg.len() - 1;
                        break;
                    }
                }
                index += if consumes_next { 2 } else { 1 };
                continue;
            }
        }
        if arg.contains('=')
            || matches!(
                arg.as_str(),
                "-n" | "-i"
                    | "-l"
                    | "-L"
                    | "-F"
                    | "-w"
                    | "-x"
                    | "-v"
                    | "-q"
                    | "--files"
                    | "--hidden"
                    | "--no-ignore"
                    | "--ignore-case"
                    | "--line-number"
                    | "--fixed-strings"
                    | "--files-with-matches"
            )
        {
            index += 1;
        } else {
            // An unknown option may consume the next argument. Let the actual
            // program interpret it instead of guessing at a later repair.
            break;
        }
    }

    Ok(())
}

fn lint_rg_literal_glob_paths(
    argv: &[String],
    shell_type: Option<ShellType>,
) -> Result<(), CommandPreflightIssue> {
    if (!cfg!(windows) && shell_type.is_none())
        || matches!(
            shell_type,
            Some(ShellType::Bash | ShellType::Zsh | ShellType::Sh)
        )
    {
        return Ok(());
    }

    // Share argument roles with search scoping so explicit patterns, short
    // option clusters, and metadata modes cannot be mistaken for path operands.
    if !argv.first().is_some_and(|program| is_rg_program(program)) {
        return Ok(());
    }
    for arg in rg_search_path_operands(&[argv.to_vec()]).unwrap_or_default() {
        if looks_like_unexpanded_glob_path(&arg) {
            let detail = match shell_type {
                Some(ShellType::PowerShell) => format!(
                    "PowerShell does not POSIX-expand native-command wildcard path arguments; `rg` receives `{arg}` as a literal path."
                ),
                Some(ShellType::Cmd) => format!(
                    "cmd does not POSIX-expand native-command wildcard path arguments; `rg` receives `{arg}` as a literal path."
                ),
                _ => format!(
                    "`rg` direct argv path arguments are not shell-expanded; `{arg}` is passed as a literal path."
                ),
            };
            return Err(CommandPreflightIssue::reject(
                CommandPreflightIssueCode::RgLiteralGlobPath,
                CommandPreflightRejected::Argv(argv.to_vec()),
                detail,
                Some(
                    "search the parent directory and pass wildcards through `--glob`, for example `rg --files .codex/skills --glob \"**/SKILL.md\"`."
                        .to_string(),
                ),
                None,
            ));
        }
    }

    Ok(())
}

fn lint_direct_argv_powershell_cmdlet(
    argv: &[String],
    shell_type: Option<ShellType>,
) -> Result<(), CommandPreflightIssue> {
    if shell_type.is_some() {
        return Ok(());
    }

    if argv
        .first()
        .is_some_and(|program| program.contains(['/', '\\']))
    {
        return Ok(());
    }

    let Some(program) = argv.first().map(|program| program_name(program)) else {
        return Ok(());
    };

    // Direct argv can legitimately target an executable whose name happens to
    // have PowerShell's Verb-Noun shape. Reject only names that are actually in
    // the cmdlet/alias allowlist; the broader shape heuristic is useful only
    // while diagnosing a script for the wrong shell.
    if is_known_powershell_cmdlet(program) || is_known_powershell_alias(program) {
        return Err(CommandPreflightIssue::reject(
            CommandPreflightIssueCode::DirectArgvPowerShellCmdlet,
            CommandPreflightRejected::Argv(argv.to_vec()),
            format!(
                "`{program}` also names a PowerShell cmdlet or alias. If no native executable resolves, use explicit PowerShell source; the direct invocation is unchanged."
            ),
            None,
            Some(CommandPreflightRetry::PowerShellScript {
                script_body: powershell_join_args(argv),
            }),
        ));
    }

    Ok(())
}

fn known_flag_fix<'a>(program: &str, arg: &'a str) -> Option<(&'a str, &'static str)> {
    let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
    let program_lower = program.to_ascii_lowercase();
    let flag_lower = flag.to_ascii_lowercase();
    match (program_lower.as_str(), flag_lower.as_str()) {
        ("rg" | "rga", "--ignorecase") => Some((flag, "--ignore-case")),
        ("rg" | "rga", "--files-with-match") => Some((flag, "--files-with-matches")),
        ("grep", "--ignorecase") => Some((flag, "--ignore-case")),
        ("cargo", "--pakage") => Some((flag, "--package")),
        ("npm", "--workpace") => Some((flag, "--workspace")),
        ("pytest", "--max-fail") => Some((flag, "--maxfail")),
        ("get-childitem", "-recuse") => Some((flag, "-Recurse")),
        ("select-string", "-patern") => Some((flag, "-Pattern")),
        ("select-string", "-casesensitve") => Some((flag, "-CaseSensitive")),
        _ => None,
    }
}

fn suggested_argv(argv: &[String], index: usize, good: &str) -> Vec<String> {
    let mut suggested = argv.to_vec();
    suggested[index] = argv[index]
        .split_once('=')
        .map_or_else(|| good.to_string(), |(_, value)| format!("{good}={value}"));
    suggested
}

fn retry_argv_from_command(command: &[String]) -> Option<CommandPreflightRetry> {
    let (program, args) = command.split_first()?;
    Some(CommandPreflightRetry::Argv {
        program: program.clone(),
        args: args.to_vec(),
    })
}

fn powershell_join_args(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if is_bare_powershell_arg(arg) {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_bare_powershell_arg(arg: &str) -> bool {
    !arg.is_empty()
        && arg
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | '=' | ':'))
}

pub(super) fn is_rg_program(program: &str) -> bool {
    matches_ignore_ascii_case(program_name(program), &["rg", "rga", "ripgrep"])
}

pub(super) fn rg_option_consumes_next(arg: &str) -> bool {
    matches!(
        arg,
        "-A" | "--after-context"
            | "-B"
            | "--before-context"
            | "-C"
            | "--context"
            | "--context-separator"
            | "--color"
            | "--colors"
            | "-d"
            | "--max-depth"
            | "--dfa-size-limit"
            | "-e"
            | "--regexp"
            | "-E"
            | "--encoding"
            | "--engine"
            | "-f"
            | "--file"
            | "--field-context-separator"
            | "--field-match-separator"
            | "-g"
            | "--glob"
            | "--iglob"
            | "--ignore-file"
            | "--hostname-bin"
            | "--hyperlink-format"
            | "-j"
            | "--threads"
            | "-m"
            | "--max-count"
            | "-M"
            | "--max-columns"
            | "--max-filesize"
            | "--path-separator"
            | "--pre"
            | "--pre-glob"
            | "-r"
            | "--replace"
            | "--regex-size-limit"
            | "--sort"
            | "--sortr"
            | "-t"
            | "--type"
            | "--type-add"
            | "--type-clear"
            | "-T"
            | "--type-not"
    )
}

fn looks_like_unexpanded_glob_path(arg: &str) -> bool {
    (arg.contains('*') || arg.contains('?'))
        && !arg.starts_with("http://")
        && !arg.starts_with("https://")
}

pub(super) fn program_name(program: &str) -> &str {
    let program = strip_matching_quotes(program.trim());
    let file_name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let file_name = strip_matching_quotes(file_name);
    match file_name.rsplit_once('.') {
        Some((stem, extension)) if is_windows_executable_extension(extension) => stem,
        _ => file_name,
    }
}

fn strip_matching_quotes(value: &str) -> &str {
    if value.len() < 2 {
        return value;
    }
    let bytes = value.as_bytes();
    if matches!(
        (bytes.first(), bytes.last()),
        (Some(b'"'), Some(b'"')) | (Some(b'\''), Some(b'\''))
    ) {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn is_windows_executable_extension(extension: &str) -> bool {
    matches_ignore_ascii_case(extension, &["bat", "cmd", "com", "exe"])
}

fn is_known_powershell_cmdlet(command: &str) -> bool {
    matches_ignore_ascii_case(
        command,
        &[
            "Get-ChildItem",
            "Get-Content",
            "Set-Content",
            "Select-String",
            "Remove-Item",
            "Move-Item",
            "Copy-Item",
            "New-Item",
            "Test-Path",
            "Resolve-Path",
            "Start-Process",
            "Invoke-WebRequest",
            "Invoke-RestMethod",
        ],
    )
}

fn is_known_powershell_alias(command: &str) -> bool {
    matches_ignore_ascii_case(
        command,
        &[
            "gal", "gci", "gcm", "gc", "gl", "gp", "gps", "gu", "gv", "gwmi", "ii", "irm", "iwr",
            "mi", "ni", "ri", "rvpa", "saps", "sp",
        ],
    )
}

pub(super) fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn truncate(value: &str) -> String {
    const MAX: usize = 240;
    if value.len() <= MAX {
        value.to_string()
    } else {
        let mut end = MAX;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &value[..end])
    }
}

