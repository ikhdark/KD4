use std::path::Path;

use codex_protocol::parse_command::ParsedCommand;
use codex_shell_command::bash::parse_shell_script_into_commands;
use codex_shell_command::is_safe_command::is_known_safe_command;
use codex_shell_command::parse_command::parse_shell_script;
use codex_utils_absolute_path::AbsolutePathBuf;

pub const MEMORIES_USAGE_METRIC: &str = "codex.memories.usage";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemoriesUsageKind {
    MemoryMd,
    MemorySummary,
    RawMemories,
    RolloutSummaries,
    Skills,
}

impl MemoriesUsageKind {
    pub fn as_tag(self) -> &'static str {
        match self {
            Self::MemoryMd => "memory_md",
            Self::MemorySummary => "memory_summary",
            Self::RawMemories => "raw_memories",
            Self::RolloutSummaries => "rollout_summaries",
            Self::Skills => "skills",
        }
    }
}

pub fn memories_usage_kinds_from_command(
    command: &str,
    cwd: &AbsolutePathBuf,
    memory_root: &AbsolutePathBuf,
) -> Vec<MemoriesUsageKind> {
    let Some(commands) = parse_shell_script_into_commands(command) else {
        return Vec::new();
    };
    if !commands
        .iter()
        .all(|command| is_known_safe_command(command))
    {
        return Vec::new();
    }

    parse_shell_script(command)
        .into_iter()
        .filter_map(|command| match command {
            ParsedCommand::Read { path, .. } => get_memory_kind(&path, cwd, memory_root),
            ParsedCommand::Search { path, .. } => {
                path.and_then(|path| get_memory_kind(&path, cwd, memory_root))
            }
            ParsedCommand::ListFiles { .. } | ParsedCommand::Unknown { .. } => None,
        })
        .collect()
}

fn get_memory_kind(
    path: impl AsRef<Path>,
    cwd: &AbsolutePathBuf,
    memory_root: &AbsolutePathBuf,
) -> Option<MemoriesUsageKind> {
    // Resolve lexically: telemetry must not issue filesystem reads or depend on
    // whether the command succeeded. Native Windows separators are supported by
    // AbsolutePathBuf; on Unix a backslash remains a literal filename character.
    let path = AbsolutePathBuf::resolve_path_against_base(path, cwd);
    let relative = path.as_path().strip_prefix(memory_root.as_path()).ok()?;
    let mut components = relative.components();
    let first = components.next()?.as_os_str().to_str()?;
    let has_child = components.next().is_some();
    match (first, has_child) {
        ("MEMORY.md", false) => Some(MemoriesUsageKind::MemoryMd),
        ("memory_summary.md", false) => Some(MemoriesUsageKind::MemorySummary),
        ("raw_memories.md", false) => Some(MemoriesUsageKind::RawMemories),
        ("rollout_summaries", _) => Some(MemoriesUsageKind::RolloutSummaries),
        ("skills", _) => Some(MemoriesUsageKind::Skills),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_reads_and_searches_only_under_the_configured_root() {
        let cwd = AbsolutePathBuf::current_dir().unwrap();
        let root = cwd.join("home/memories");
        for (command, expected) in [
            (
                "cat home/memories/MEMORY.md",
                vec![MemoriesUsageKind::MemoryMd],
            ),
            ("cat home/memories/MEMORY.md.bak", vec![]),
            ("cat other/memories/MEMORY.md", vec![]),
            ("cat home/notmemories/MEMORY.md", vec![]),
            ("cat home/memories/../MEMORY.md", vec![]),
            (
                "cat home/memories/skills/example/SKILL.md",
                vec![MemoriesUsageKind::Skills],
            ),
            (
                "rg needle home/memories/rollout_summaries",
                vec![MemoriesUsageKind::RolloutSummaries],
            ),
            ("cat home/memories/MEMORY.md; touch changed", vec![]),
        ] {
            assert_eq!(
                memories_usage_kinds_from_command(command, &cwd, &root),
                expected,
                "{command}"
            );
        }
        assert_eq!(
            memories_usage_kinds_from_command("cat MEMORY.md", &root, &root),
            vec![MemoriesUsageKind::MemoryMd]
        );
        assert_eq!(
            memories_usage_kinds_from_command("cat ./memory_summary.md", &root, &root),
            vec![MemoriesUsageKind::MemorySummary]
        );
    }

    #[cfg(windows)]
    #[test]
    fn classifies_quoted_native_windows_paths() {
        let cwd = AbsolutePathBuf::current_dir().unwrap();
        let root = cwd.join("memory home/memories");
        let command = format!("cat '{}'", root.join("raw_memories.md").display());
        assert_eq!(
            memories_usage_kinds_from_command(&command, &cwd, &root),
            vec![MemoriesUsageKind::RawMemories]
        );
    }
}
