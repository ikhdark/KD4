//! Shared helper utilities for Windows sandbox setup.
//!
//! These helpers centralize small pieces of setup logic used across both legacy and
//! elevated paths, including unified_exec sessions and capture flows. They cover
//! codex home directory creation and git safe.directory injection so sandboxed
//! users can run git inside a repo owned by the primary user.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

/// Walk upward from `start` to locate the git worktree root for `safe.directory`.
fn find_git_worktree_root_for_safe_directory(start: &Path) -> Option<std::path::PathBuf> {
    let mut cur = dunce::canonicalize(start).ok()?;
    loop {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        let parent = cur.parent()?;
        if parent == cur {
            return None;
        }
        cur = parent.to_path_buf();
    }
}

/// Ensure the sandbox codex home directory exists.
pub fn ensure_codex_home_exists(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p)?;
    Ok(())
}

/// Adds a git safe.directory entry to the environment when running inside a repository.
/// git will not otherwise allow the Sandbox user to run git commands on the repo directory
/// which is owned by the primary user.
pub fn inject_git_safe_directory(env_map: &mut HashMap<String, String>, cwd: &Path) {
    if let Some(git_root) = find_git_worktree_root_for_safe_directory(cwd) {
        let cfg_count = match env_map.get("GIT_CONFIG_COUNT") {
            Some(value) => match value.parse::<usize>() {
                Ok(count) => count,
                Err(_) => return,
            },
            None => 0,
        };
        let Some(next_count) = cfg_count.checked_add(1) else {
            return;
        };
        let git_path = git_root.to_string_lossy().replace('\\', "/");
        env_map.insert(
            format!("GIT_CONFIG_KEY_{cfg_count}"),
            "safe.directory".to_string(),
        );
        env_map.insert(format!("GIT_CONFIG_VALUE_{cfg_count}"), git_path);
        env_map.insert("GIT_CONFIG_COUNT".to_string(), next_count.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::inject_git_safe_directory;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn safe_directory_value(path: &Path) -> String {
        let value = dunce::canonicalize(path)
            .expect("canonicalize path")
            .to_string_lossy()
            .replace('\\', "/");
        // Git reports worktree roots with forward slashes; pin that form literally.
        assert!(
            value.ends_with("/repo") && !value.contains('\\'),
            "{value}"
        );
        value
    }

    #[test]
    fn preserves_invalid_or_exhausted_git_config_count() {
        let repo = TempDir::new().expect("tempdir");
        fs::create_dir(repo.path().join(".git")).expect("create .git");
        for count in ["invalid".to_string(), usize::MAX.to_string()] {
            let original = HashMap::from([
                ("GIT_CONFIG_COUNT".to_string(), count),
                ("GIT_CONFIG_KEY_0".to_string(), "existing.key".to_string()),
                (
                    "GIT_CONFIG_VALUE_0".to_string(),
                    "existing value".to_string(),
                ),
            ]);
            let mut env_map = original.clone();

            inject_git_safe_directory(&mut env_map, repo.path());

            assert_eq!(env_map, original);
        }
    }

    #[test]
    fn injects_worktree_root_without_overwriting_existing_config() {
        for git_is_file in [false, true] {
            let temp = TempDir::new().expect("tempdir");
            let repo = temp.path().join("repo");
            let nested = repo.join("nested");
            fs::create_dir_all(&nested).expect("create nested dir");
            if git_is_file {
                fs::write(
                    repo.join(".git"),
                    "gitdir: C:/Users/example/repo/.git/worktrees/codex3\n",
                )
                .expect("write .git file");
            } else {
                fs::create_dir(repo.join(".git")).expect("create .git");
            }

            for existing_count in [0, 1] {
                let mut expected = HashMap::new();
                if existing_count == 1 {
                    expected.extend([
                        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
                        ("GIT_CONFIG_KEY_0".to_string(), "existing.key".to_string()),
                        ("GIT_CONFIG_VALUE_0".to_string(), "existing value".to_string()),
                    ]);
                }
                let mut env_map = expected.clone();
                inject_git_safe_directory(&mut env_map, &nested);

                expected.extend([
                    ("GIT_CONFIG_COUNT".to_string(), (existing_count + 1).to_string()),
                    (format!("GIT_CONFIG_KEY_{existing_count}"), "safe.directory".to_string()),
                    (format!("GIT_CONFIG_VALUE_{existing_count}"), safe_directory_value(&repo)),
                ]);
                assert_eq!(env_map, expected, "gitfile={git_is_file}, count={existing_count}");
            }
        }
    }
}
