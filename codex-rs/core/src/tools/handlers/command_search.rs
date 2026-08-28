use std::path::Path;
use std::path::PathBuf;

use crate::shell::ShellType;

use super::command_preflight::infer_direct_shell_type;
use super::command_preflight::matches_ignore_ascii_case;
use super::command_preflight::program_name;
use super::command_preflight::rg_argv_commands;
use super::command_preflight::rg_option_consumes_next;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub(crate) enum RgSearchBreadth {
    Narrow,
    Broad,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RgSearchNarrowing {
    pub(crate) breadth: RgSearchBreadth,
    pub(crate) query_identity: String,
    pub(crate) search_identity: String,
    pub(crate) scope_identity: String,
    pub(crate) parent_scope_identity: Option<String>,
    pub(crate) can_record_miss: bool,
}

pub(crate) fn classify_rg_search_narrowing(
    command: &[String],
    shell_type: Option<ShellType>,
    cwd: &Path,
    repository_root: &Path,
) -> Result<Option<RgSearchNarrowing>, String> {
    let commands = rg_argv_commands(command, shell_type)?;
    let rg_commands = commands
        .iter()
        .filter(|argv| {
            argv.first()
                .map(|program| program_name(program))
                .is_some_and(|program| matches_ignore_ascii_case(program, &["rg", "rga"]))
                && rg_invocation_searches(argv)
        })
        .collect::<Vec<_>>();
    if rg_commands.is_empty() {
        return Ok(None);
    }
    let repository_root = normalized_search_path(repository_root);
    let mut query_identities = Vec::new();
    let mut search_identities = Vec::new();
    let mut all_targets = Vec::new();
    let mut owner_scopes = Vec::new();
    let mut repository_wide = false;
    for argv in &rg_commands {
        let path_indices = rg_path_operand_indices(argv);
        let query_identity = rg_query_identity(argv, &path_indices);
        let mut targets = if path_indices.is_empty() {
            vec![normalized_search_path(cwd)]
        } else {
            path_indices
                .iter()
                .map(|index| {
                    let path = Path::new(&argv[*index]);
                    let target = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        cwd.join(path)
                    };
                    normalized_search_path(&target)
                })
                .collect::<Vec<_>>()
        };
        targets.sort_unstable();
        targets.dedup();
        for target in &targets {
            match repository_owner_scope(target, &repository_root) {
                Some(owner_scope) => owner_scopes.push(owner_scope),
                None => repository_wide = true,
            }
        }
        all_targets.extend(targets.iter().cloned());
        search_identities.push(format!(
            "{}\u{1d}{query_identity}\u{1d}{}",
            program_name(&argv[0]),
            targets
                .iter()
                .map(|target| target.to_string_lossy())
                .collect::<Vec<_>>()
                .join("\u{1c}")
        ));
        query_identities.push(query_identity);
    }
    all_targets.sort_unstable();
    all_targets.dedup();
    owner_scopes.sort_unstable();
    owner_scopes.dedup();
    repository_wide |= owner_scopes.len() > 1;
    let scope_identity = path_scope_identity(&all_targets);
    let parent_scope_identity = parent_scope_identity(&all_targets, &repository_root);
    Ok(Some(RgSearchNarrowing {
        breadth: if repository_wide {
            RgSearchBreadth::Broad
        } else {
            RgSearchBreadth::Narrow
        },
        query_identity: query_identities.join("\u{1f}"),
        search_identity: search_identities.join("\u{1f}"),
        scope_identity,
        parent_scope_identity,
        // A compound process exit code cannot be attributed to its rg command.
        can_record_miss: commands.len() == 1 && rg_commands.len() == 1,
    }))
}

pub(crate) fn reject_rg_search_without_native_scope(
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<(), String> {
    let commands = rg_argv_commands(command, shell_type)?;
    if commands.iter().any(|argv| {
        argv.first()
            .map(|program| program_name(program))
            .is_some_and(|program| matches_ignore_ascii_case(program, &["rg", "rga"]))
            && rg_invocation_searches(argv)
    }) {
        return Err(
            "`rg` search rejected because the environment workdir cannot be mapped to a native repository path. Use a native repository workdir so owner-scoped search breadth can be verified."
                .to_string(),
        );
    }
    Ok(())
}

fn rg_invocation_searches(argv: &[String]) -> bool {
    !argv.iter().skip(1).any(|arg| {
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        matches!(
            flag,
            "-h" | "--help" | "-V" | "--version" | "--type-list" | "--pcre2-version" | "--generate"
        )
    })
}

fn rg_query_identity(argv: &[String], path_indices: &[usize]) -> String {
    argv.iter()
        .enumerate()
        .filter(|(index, _)| *index != 0 && !path_indices.contains(index))
        .map(|(_, argument)| argument.as_str())
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

fn repository_owner_scope(target: &Path, repository_root: &Path) -> Option<PathBuf> {
    let relative = target.strip_prefix(repository_root).ok()?;
    let mut components = relative.components();
    let first = components.next()?.as_os_str();
    if first.eq_ignore_ascii_case("codex-rs") {
        let second = components.next()?.as_os_str();
        Some(repository_root.join(first).join(second))
    } else {
        Some(repository_root.join(first))
    }
}

fn path_scope_identity(targets: &[PathBuf]) -> String {
    targets
        .iter()
        .map(|target| target.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\u{1c}")
}

fn parent_scope_identity(targets: &[PathBuf], repository_root: &Path) -> Option<String> {
    let mut parents = Vec::new();
    for target in targets {
        if target == repository_root || !target.starts_with(repository_root) {
            return None;
        }
        parents.push(target.parent()?.to_path_buf());
    }
    parents.sort_unstable();
    parents.dedup();
    Some(path_scope_identity(&parents))
}

fn normalized_search_path(path: &Path) -> PathBuf {
    #[cfg(test)]
    SEARCH_PATH_NORMALIZATION_COUNT.with(|count| count.set(count.get() + 1));
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                component => normalized.push(component.as_os_str()),
            }
        }
        normalized
    })
}

#[cfg(test)]
thread_local! {
    static SEARCH_PATH_NORMALIZATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_search_path_normalization_count() {
    SEARCH_PATH_NORMALIZATION_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn search_path_normalization_count() -> usize {
    SEARCH_PATH_NORMALIZATION_COUNT.with(std::cell::Cell::get)
}

pub(crate) fn rg_search_path_operands(
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<Option<Vec<String>>, String> {
    let commands = rg_argv_commands(
        command,
        shell_type.or_else(|| infer_direct_shell_type(command)),
    )?;
    let mut saw_search = false;
    let mut operands = Vec::new();
    for argv in commands {
        let is_rg_search = argv
            .first()
            .map(|program| program_name(program))
            .is_some_and(|program| matches_ignore_ascii_case(program, &["rg", "rga", "ripgrep"]))
            && rg_invocation_searches(&argv);
        if !is_rg_search {
            continue;
        }
        saw_search = true;
        operands.extend(
            rg_path_operand_indices(&argv)
                .into_iter()
                .filter_map(|index| argv.get(index).cloned()),
        );
    }
    Ok(saw_search.then_some(operands))
}

fn rg_path_operand_indices(argv: &[String]) -> Vec<usize> {
    let files_mode = argv.iter().skip(1).any(|arg| arg == "--files");
    let explicit_pattern = argv.iter().skip(1).any(|arg| {
        matches!(arg.as_str(), "-e" | "--regexp" | "-f" | "--file")
            || (arg.starts_with("-e") && arg.len() > 2)
            || arg.starts_with("--regexp=")
            || (arg.starts_with("-f") && arg.len() > 2)
            || arg.starts_with("--file=")
    });
    let mut positional = Vec::new();
    let mut options_finished = false;
    let mut index = 1usize;
    while let Some(arg) = argv.get(index) {
        if !options_finished && arg == "--" {
            options_finished = true;
            index += 1;
            continue;
        }
        if !options_finished && rg_option_consumes_next(arg) {
            index += 2;
            continue;
        }
        if !options_finished && arg.starts_with('-') {
            index += 1;
            continue;
        }
        positional.push(index);
        index += 1;
    }
    if files_mode || explicit_pattern {
        positional
    } else {
        positional.into_iter().skip(1).collect()
    }
}
