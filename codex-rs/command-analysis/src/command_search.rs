use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

use codex_shell_command::shell_detect::ShellType;

use super::command_preflight::is_rg_program;
use super::command_preflight::rg_argv_commands;
use super::command_preflight::rg_option_consumes_next;

/// Classify an attributable search exit without filesystem probes or result reuse.
pub fn rg_search_no_match_eligible(command: &[String], shell_type: Option<ShellType>) -> bool {
    let Ok(commands) = rg_argv_commands(command, shell_type) else { return false };
    let [argv] = commands.as_slice() else { return false };
    if !argv.first().is_some_and(|program| is_rg_program(program)) { return false; }
    let roles = RgArgumentRoles::parse(argv);
    roles.searches && roles.unresolved_file_inputs(argv).is_empty()
}

struct SearchDiagnosticBudget {
    remaining: usize,
    deadline: Instant,
    cancellation: CancellationToken,
}

impl SearchDiagnosticBudget {
    fn check(&mut self) -> io::Result<()> {
        if self.remaining == 0
            || Instant::now() >= self.deadline
            || self.cancellation.is_cancelled()
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "search diagnostic budget exhausted",
            ));
        }
        self.remaining -= 1;
        Ok(())
    }
}

fn search_state_paths(
    targets: &[PathBuf],
    explicit_input_files: &[PathBuf],
    repository_root: &Path,
) -> Vec<PathBuf> {
    let mut paths = targets.to_vec();
    paths.extend(explicit_input_files.iter().cloned());
    let git_entry = repository_root.join(".git");
    // A linked worktree's .git file names its private metadata directory;
    // info/exclude belongs to the shared directory named by commondir.
    let git_dir = if git_entry.is_file() {
        paths.push(git_entry.clone());
        std::fs::read_to_string(&git_entry)
            .ok()
            .and_then(|text| text.trim().strip_prefix("gitdir: ").map(str::to_owned))
            .map(|path| repository_root.join(path))
            .unwrap_or_else(|| git_entry.clone())
    } else {
        git_entry
    };
    let common_dir_file = git_dir.join("commondir");
    paths.push(common_dir_file.clone());
    let common_dir = std::fs::read_to_string(common_dir_file)
        .map(|path| git_dir.join(path.trim()))
        .unwrap_or(git_dir);
    paths.push(common_dir.join("info").join("exclude"));
    for target in targets {
        let mut ancestor = if target.is_dir() {
            Some(target.as_path())
        } else {
            target.parent()
        };
        while let Some(directory) = ancestor {
            if !directory.starts_with(repository_root) {
                break;
            }
            for ignore_name in [".gitignore", ".ignore", ".rgignore"] {
                paths.push(directory.join(ignore_name));
            }
            if directory == repository_root {
                break;
            }
            ancestor = directory.parent();
        }
    }
    paths.sort_unstable();
    paths.dedup();
    paths
}

#[derive(Debug)]
pub struct RgSearchDependencies {
    pub paths: Vec<PathBuf>,
    pub unresolved_inputs: Vec<&'static str>,
}

impl RgSearchDependencies {
    pub fn is_complete(&self) -> bool {
        self.unresolved_inputs.is_empty()
    }
}

/// Files affecting a search result, not merely the searched path operands.
/// Keep option inputs and ancestor ignore rules in the same existing scope
/// owner used by history; preflight still consumes operands alone.
pub fn rg_search_dependency_paths(argv: &[String], cwd: &Path) -> Option<RgSearchDependencies> {
    let roles = RgArgumentRoles::parse(argv);
    // `None` means the invocation reads nothing. A stdin-capable search still
    // reads files (without a readable stdin, rg walks the cwd), so it keeps
    // its targets and reports stdin as an unresolved input instead.
    if !roles.searches {
        return None;
    }
    let mut unresolved_inputs = roles.unresolved_file_inputs(argv);
    // History has no effective child environment. Require explicit opt-out of
    // ambient RIPGREP_CONFIG_PATH rather than assuming the host's environment.
    if !roles.no_config {
        unresolved_inputs.push("ambient_ripgrep_configuration");
    }
    let targets = if roles.path_indices.is_empty() {
        vec![cwd.to_path_buf()]
    } else {
        roles
            .path_indices
            .iter()
            .map(|index| cwd.join(&argv[*index]))
            .collect()
    };
    let inputs = roles
        .input_files
        .iter()
        .map(|path| cwd.join(path))
        .collect::<Vec<_>>();
    // Explicit file operands are searched even when ignored. Directory walks
    // additionally depend on the ignore files that decide their membership.
    let directories = targets
        .iter()
        .filter(|path| path.is_dir() || (!path.is_file() && path.extension().is_none()))
        .cloned()
        .collect::<Vec<_>>();
    if directories.is_empty() {
        return Some(RgSearchDependencies {
            paths: targets.into_iter().chain(inputs).collect(),
            unresolved_inputs,
        });
    }
    let root = codex_git_utils::get_git_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let mut paths = search_state_paths(&directories, &inputs, &root);
    paths.extend(targets);
    paths.sort_unstable();
    paths.dedup();
    Some(RgSearchDependencies {
        paths,
        unresolved_inputs,
    })
}

pub fn rg_search_path_operands(commands: &[Vec<String>]) -> Option<Vec<String>> {
    let mut saw_search = false;
    let mut operands = Vec::new();
    for argv in commands {
        if !argv.first().is_some_and(|program| is_rg_program(program)) {
            continue;
        }
        let roles = RgArgumentRoles::parse(argv);
        if !roles.searches {
            continue;
        }
        saw_search = true;
        operands.extend(
            roles
                .path_indices
                .into_iter()
                .filter_map(|index| argv.get(index).cloned()),
        );
    }
    saw_search.then_some(operands)
}

pub fn rg_search_path_indices(argv: &[String]) -> Option<Vec<usize>> {
    let roles = RgArgumentRoles::parse(argv);
    roles.searches.then_some(roles.path_indices)
}

/// Bounded advisory candidates only; no search results are inferred from this
/// directory walk. The caller excludes Git-ignored candidates separately.
pub fn skipped_hidden_rg_directories(script: &str, cwd: &Path) -> Vec<PathBuf> {
    skipped_hidden_rg_directories_with_cancellation(script, cwd, CancellationToken::new())
}

pub fn skipped_hidden_rg_directories_with_cancellation(
    script: &str,
    cwd: &Path,
    cancellation: CancellationToken,
) -> Vec<PathBuf> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(25);
    let Some(argv) = codex_shell_command::validation::standalone_argv(script) else {
        return Vec::new();
    };
    if !argv.first().is_some_and(|program| is_rg_program(program)) {
        return Vec::new();
    }
    let roles = RgArgumentRoles::parse(&argv);
    if !roles.searches || roles.hidden {
        return Vec::new();
    }
    let roots = if roles.path_indices.is_empty() {
        vec![cwd.to_path_buf()]
    } else {
        roles
            .path_indices
            .iter()
            .map(|index| cwd.join(&argv[*index]))
            .collect()
    };
    let mut found = std::collections::BTreeSet::new();
    let mut inspected = 0;
    for root in roots {
        if cancellation.is_cancelled() || std::time::Instant::now() >= deadline {
            break;
        }
        if !root.is_dir() {
            continue;
        }
        let mut walk = walkdir::WalkDir::new(&root)
            .follow_links(false)
            .into_iter();
        loop {
            if cancellation.is_cancelled()
                || std::time::Instant::now() >= deadline
                || inspected >= 2048
                || found.len() >= 16
            {
                return found.into_iter().collect();
            }
            let Some(entry) = walk.next() else { break };
            inspected += 1;
            let Ok(entry) = entry else { continue };
            if entry.depth() == 0 || !entry.file_type().is_dir() {
                continue;
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                walk.skip_current_dir();
                if entry.file_name() != ".git" {
                    found.insert(entry.into_path());
                }
            }
        }
    }
    found.into_iter().collect()
}

const MAX_MISSING_PATH_NOTES: usize = 3;

const MAX_PATH_SUGGESTIONS: usize = 3;

const MAX_SUGGESTION_DEPTH: usize = 3;
const MAX_SUGGESTION_ENTRIES: usize = 4_000;
const MAX_SUGGESTION_CLIMBS: usize = 2;


pub fn missing_rg_path_advisory_with_cancellation(
    output: &str,
    cwd: &Path,
    cancellation: CancellationToken,
) -> Option<String> {
    let mut budget = suggestion_budget(cancellation);
    missing_rg_path_advisory_with_budget(output, cwd, &mut budget)
}

fn missing_rg_path_advisory_with_budget(
    output: &str,
    cwd: &Path,
    budget: &mut SearchDiagnosticBudget,
) -> Option<String> {
    #[expect(
        clippy::expect_used,
        reason = "the fixed missing-path regex is valid and exercised by advisory tests"
    )]
    static MISSING_PATH: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
        regex_lite::Regex::new(r"(?m)^rg: (.+?): [^\r\n]*\(os error [23]\)\r?$")
            .expect("valid rg missing-path regex")
    });
    let mut reported = Vec::<&str>::new();
    for capture in MISSING_PATH.captures_iter(output) {
        if budget.check().is_err() {
            break;
        }
        let path = capture.get(1).map_or("", |path| path.as_str()).trim();
        if !path.is_empty() && !reported.contains(&path) {
            reported.push(path);
        }
    }
    let notes = reported
        .into_iter()
        .filter_map(|reported| {
            let missing = cwd.join(reported);
            // A script can search from another directory; never contradict it.
            if missing.exists() {
                return None;
            }
            let suggestions = nearest_existing_paths_with_budget(&missing, cwd, budget);
            (!suggestions.is_empty()).then(|| {
                let suggestions = suggestions
                    .iter()
                    .map(|path| format!("`{}`", display_suggestion(path, cwd)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("`{reported}` -> {suggestions}")
            })
        })
        .take(MAX_MISSING_PATH_NOTES)
        .collect::<Vec<_>>();
    (!notes.is_empty()).then(|| {
        format!(
            "Hint: `rg` could not open these search paths; possible existing paths (not verified replacements) are {}. The other paths still ran, and a path error is not evidence of no matches.",
            notes.join("; ")
        )
    })
}

/// Search below the deepest existing ancestor, climbing at most two levels and
/// never above the working directory for a path requested inside it.
pub fn nearest_existing_paths(missing: &Path, cwd: &Path) -> Vec<PathBuf> {
    nearest_existing_paths_with_budget(
        missing,
        cwd,
        &mut suggestion_budget(CancellationToken::new()),
    )
}

fn suggestion_budget(cancellation: CancellationToken) -> SearchDiagnosticBudget {
    SearchDiagnosticBudget {
        remaining: MAX_SUGGESTION_ENTRIES,
        deadline: Instant::now() + Duration::from_millis(25),
        cancellation,
    }
}

fn nearest_existing_paths_with_budget(
    missing: &Path,
    cwd: &Path,
    budget: &mut SearchDiagnosticBudget,
) -> Vec<PathBuf> {
    let Some(name) = missing
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return Vec::new();
    };
    let stem = Path::new(&name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(&name)
        .to_string();
    let mut base = missing.parent();
    while let Some(directory) = base.filter(|directory| !directory.is_dir()) {
        if budget.check().is_err() {
            return Vec::new();
        }
        base = directory.parent();
    }
    let within_cwd = missing.starts_with(cwd);
    let mut climbs = 0;
    while let Some(directory) = base {
        if budget.check().is_err() {
            break;
        }
        let mut candidates = suggestion_candidates(directory, &name, &stem, budget);
        if !candidates.is_empty() {
            candidates.sort();
            return candidates
                .into_iter()
                .take(MAX_PATH_SUGGESTIONS)
                .map(|(_, _, _, path)| path)
                .collect();
        }
        if climbs == MAX_SUGGESTION_CLIMBS || (within_cwd && directory == cwd) {
            break;
        }
        climbs += 1;
        base = directory.parent();
    }
    Vec::new()
}

/// Breadth-first so a bounded walk covers the nearest levels first; ranked by
/// match strength, then depth, then name length.
fn suggestion_candidates(
    base: &Path,
    name: &str,
    stem: &str,
    budget: &mut SearchDiagnosticBudget,
) -> Vec<(std::cmp::Reverse<u8>, usize, usize, PathBuf)> {
    let mut candidates = Vec::new();
    let mut pending = std::collections::VecDeque::from([(base.to_path_buf(), 1)]);
    while let Some((directory, depth)) = pending.pop_front() {
        if budget.check().is_err() {
            break;
        }
        let Ok(mut entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        // Charge before advancing the iterator. Sorting an unbounded read_dir
        // first defeats the advisory budget on very large directories.
        loop {
            if budget.check().is_err() {
                return candidates;
            }
            let Some(entry) = entries.next() else {
                break;
            };
            let Ok(entry) = entry else {
                continue;
            };
            let Some(entry_name) = entry.file_name().to_str().map(str::to_ascii_lowercase) else {
                continue;
            };
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if let Some(score) = suggestion_score(&entry_name, is_dir, name, stem) {
                candidates.push((
                    std::cmp::Reverse(score),
                    depth,
                    entry_name.len(),
                    entry.path(),
                ));
            }
            if is_dir
                && depth < MAX_SUGGESTION_DEPTH
                && !matches!(entry_name.as_str(), ".git" | "target" | "node_modules")
            {
                pending.push_back((entry.path(), depth + 1));
            }
        }
    }
    candidates
}

/// Exact names first, then the same stem (`code_mode.rs` -> `code_mode/`),
/// then a stem that extends or shortens the request (`spec` -> `spec_plan`).
fn suggestion_score(candidate: &str, is_dir: bool, name: &str, stem: &str) -> Option<u8> {
    if candidate == name {
        return Some(3);
    }
    let candidate_stem = if is_dir {
        candidate
    } else {
        Path::new(candidate)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(candidate)
    };
    if candidate_stem == stem {
        return Some(2);
    }
    let extends = |longer: &str, shorter: &str| {
        shorter.len() >= 3
            && longer
                .strip_prefix(shorter)
                .is_some_and(|rest| rest.starts_with(['_', '-', '.']))
    };
    (extends(candidate_stem, stem) || extends(stem, candidate_stem)).then_some(1)
}

pub(super) fn display_suggestion(path: &Path, cwd: &Path) -> String {
    let mut display = path.strip_prefix(cwd).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.to_string_lossy().replace('\\', "/"),
    );
    if path.is_dir() {
        display.push('/');
    }
    display
}

struct RgArgumentRoles<'a> {
    hidden: bool,
    unrestricted: usize,
    path_indices: Vec<usize>,
    input_files: Vec<&'a str>,
    searches: bool,
    follows_links: bool,
    no_ignore_global: bool,
    no_ignore_parent: bool,
    no_config: bool,
    files_mode: bool,
    type_list: bool,
    explicit_pattern: bool,
    uses_stdin: bool,
}

impl<'a> RgArgumentRoles<'a> {
    fn unresolved_file_inputs(&self, argv: &[String]) -> Vec<&'static str> {
        let mut unresolved = Vec::new();
        for (present, reason) in [
            (self.uses_stdin, "stdin"),
            (self.follows_links, "symlink_targets"),
            (!self.no_ignore_global, "global_ignore_configuration"),
            (!self.no_ignore_parent, "parent_ignore_files"),
            (
                !codex_shell_command::is_safe_command::is_known_safe_direct_argv(argv),
                "unsupported_transform_or_options",
            ),
        ] {
            if present {
                unresolved.push(reason);
            }
        }
        unresolved
    }

    fn option(&mut self, flag: &str, value: Option<&'a str>) {
        match flag {
            "--hidden" => self.hidden = true,
            "--no-hidden" => self.hidden = false,
            "-u" | "--unrestricted" => {
                self.unrestricted += 1;
                self.no_ignore_global = true;
                self.no_ignore_parent = true;
                if self.unrestricted >= 2 {
                    self.hidden = true;
                }
            }
            "--no-ignore" => {
                self.no_ignore_global = true;
                self.no_ignore_parent = true;
            }
            "--ignore" => {
                self.no_ignore_global = false;
                self.no_ignore_parent = false;
            }
            "--no-ignore-global" => self.no_ignore_global = true,
            "--ignore-global" => self.no_ignore_global = false,
            "--no-ignore-parent" => self.no_ignore_parent = true,
            "--no-config" => self.no_config = true,
            "--ignore-parent" => self.no_ignore_parent = false,
            "--files" => self.files_mode = true,
            "--type-list" => self.type_list = true,
            "-e" | "--regexp" => self.explicit_pattern = true,
            "-f" | "--file" | "--ignore-file" => {
                self.explicit_pattern |= flag != "--ignore-file";
                if flag != "--ignore-file" && value == Some("-") {
                    self.uses_stdin = true;
                } else {
                    self.input_files.extend(value);
                }
            }
            "-L" | "--follow" => self.follows_links = true,
            "-h" | "--help" | "-V" | "--version" | "--pcre2-version"
            | "--generate" => self.searches = false,
            _ => {}
        }
    }

    fn parse(argv: &'a [String]) -> Self {
        let mut roles = Self {
            hidden: false,
            unrestricted: 0,
            path_indices: Vec::new(),
            input_files: Vec::new(),
            searches: true,
            follows_links: false,
            no_ignore_global: false,
            no_ignore_parent: false,
            no_config: false,
            files_mode: false,
            type_list: false,
            explicit_pattern: false,
            uses_stdin: false,
        };
        let mut options_finished = false;
        let mut index = 1;
        while let Some(arg) = argv.get(index) {
            if options_finished || !arg.starts_with('-') || arg == "-" {
                roles.path_indices.push(index);
            } else if arg == "--" {
                options_finished = true;
            } else if arg.starts_with("--") {
                let (flag, mut value) = arg
                    .split_once('=')
                    .map_or((arg.as_str(), None), |(flag, value)| (flag, Some(value)));
                if value.is_none() && rg_option_consumes_next(flag) {
                    index += 1;
                    value = argv.get(index).map(String::as_str);
                }
                roles.option(flag, value);
            } else {
                // A value-taking short option consumes the rest of its cluster
                // or the next argument. Neither is another option.
                for (offset, flag) in arg.char_indices().skip(1) {
                    let flag_name = format!("-{flag}");
                    if rg_option_consumes_next(&flag_name) {
                        let remainder = &arg[offset + flag.len_utf8()..];
                        let value = if remainder.is_empty() {
                            index += 1;
                            argv.get(index).map(String::as_str)
                        } else {
                            Some(remainder)
                        };
                        roles.option(&flag_name, value);
                        break;
                    }
                    roles.option(&flag_name, None);
                }
            }
            index += 1;
        }
        roles.searches &= !roles.type_list || roles.files_mode;
        if !roles.files_mode && !roles.explicit_pattern && !roles.path_indices.is_empty() {
            roles.path_indices.remove(0);
        }
        // With no explicit path, rg may select a readable stdin stream.
        roles.uses_stdin |= !roles.files_mode && roles.path_indices.is_empty();
        roles.path_indices.retain(|&index| {
            if argv[index] == "-" {
                roles.uses_stdin = true;
                false
            } else {
                true
            }
        });
        roles
    }
}



