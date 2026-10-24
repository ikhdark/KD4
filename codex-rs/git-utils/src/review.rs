//! Host-owned source scope for the retained review engine. No model-authored freshness claims.

use std::collections::HashMap;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use crate::merge_base_with_head;
use crate::operations::resolve_head;
use crate::operations::resolve_repository_root;
use crate::operations::run_git_for_stdout_exact as run_git_for_stdout;
use crate::resolve_commit;
use codex_protocol::protocol::ReviewOutputEvent;
use codex_protocol::protocol::ReviewTarget;

#[derive(Clone, Debug, PartialEq)]
pub struct ReviewSourceScope {
    root: PathBuf,
    head: Option<String>,
    comparison: Vec<String>,
    diff: String,
    staged_diff: Option<String>,
    untracked: Vec<String>,
    custom: bool,
}

impl ReviewSourceScope {
    /// Immutable revision resolved during capture, shared with prompt rendering.
    pub fn comparison_revision(&self) -> Option<&str> {
        self.comparison.last().map(String::as_str)
    }

    pub fn capture(cwd: &Path, target: &ReviewTarget) -> Result<Self, String> {
        let custom = matches!(target, ReviewTarget::Custom { .. });
        // A custom prompt can contain all its source and need not name a Git repository.
        let root = if custom {
            cwd.to_path_buf()
        } else {
            resolve_repository_root(cwd).map_err(|error| error.to_string())?
        };
        let head = if custom {
            None
        } else {
            resolve_head(&root).map_err(|error| error.to_string())?
        };
        let comparison = match target {
            ReviewTarget::UncommittedChanges => std::iter::once("diff".into())
                .chain(head.clone()).collect(),
            ReviewTarget::BaseBranch { branch } => vec![
                "diff".into(),
                merge_base_with_head(&root, branch)
                    .map_err(|error| error.to_string())?
                    .ok_or("review base could not be resolved")?,
            ],
            ReviewTarget::Commit { sha, .. } => vec![
                "show".into(),
                "--format=".into(),
                "--diff-merges=first-parent".into(),
                resolve_commit(&root, sha).map_err(|error| error.to_string())?,
            ],
            ReviewTarget::Custom { .. } => Vec::new(),
        };
        let diff = if custom {
            String::new()
        } else {
            Self::read_diff(&root, &comparison)?
        };
        let untracked = if matches!(target, ReviewTarget::UncommittedChanges) {
            run_git_for_stdout(
                &root,
                ["ls-files", "--others", "--exclude-standard", "-z"],
                None,
            )
            .map_err(|error| error.to_string())?
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect()
        } else {
            Vec::new()
        };
        let staged_diff = if matches!(target, ReviewTarget::UncommittedChanges) {
            Some(Self::read_diff(&root, &["diff".into(), "--cached".into()])?)
        } else { None };
        Ok(Self {
            root,
            head,
            comparison,
            diff,
            staged_diff,
            untracked,
            custom,
        })
    }

    fn read_diff(root: &Path, comparison: &[String]) -> Result<String, String> {
        let args = comparison.iter().map(String::as_str).chain([
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            "--unified=0",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--",
        ]);
        let diff = run_git_for_stdout(root, args, None).map_err(|error| error.to_string())?;
        if diff.len() > 8 * 1024 * 1024 {
            return Err("review diff exceeds supported 8 MiB scope".into());
        }
        Ok(diff)
    }

    /// Validate anchors and reject observable target drift. Supporting evidence is
    /// explicitly unverified: shell/prompt reads do not carry host source receipts.
    pub fn validate(&self, output: &mut ReviewOutputEvent) -> Result<(), String> {
        let historical = self
            .comparison
            .first()
            .is_some_and(|command| command == "show");
        if !self.custom
            && !historical
            && (resolve_head(&self.root).map_err(|error| error.to_string())? != self.head
                || Self::read_diff(&self.root, &self.comparison)? != self.diff
                || self.staged_diff.as_ref().is_some_and(|diff| {
                    Self::read_diff(&self.root, &["diff".into(), "--cached".into()])
                        .as_ref() != Ok(diff)
                }))
        {
            return Err(
                "review source changed after admission; no current-source verdict was published"
                    .into(),
            );
        }
        let has_locations = !self.custom && !output.findings.is_empty();
        let changes = ReviewDiffIndex::new(if has_locations { &self.diff } else { "" });
        let staged_changes = self.staged_diff.as_deref().filter(|_| has_locations)
            .map(ReviewDiffIndex::new);
        // One defined source snapshot per (side/revision, path) in this call.
        // Keep only its empty/line-count facts, not every acquired file body.
        let mut snapshots = HashMap::<(Option<String>, String), (bool, usize)>::new();
        let mut source_lines = |path: &Path, name: &str, revision: Option<&str>| -> Result<(bool, usize), String> {
            let key = (revision.map(str::to_owned), name.to_owned());
            if let Some(snapshot) = snapshots.get(&key) {
                return Ok(*snapshot);
            }
            let contents = if let Some(revision) = revision {
                run_git_for_stdout(&self.root, ["show", &format!("{revision}:{name}")], None)
                    .map_err(|error| error.to_string())?
            } else {
                // Read only returned locations, never walk the repository.
                let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
                if !metadata.is_file() || metadata.len() > 8 * 1024 * 1024 {
                    return Err("review location is not a supported regular source file".into());
                }
                let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
                if !file.metadata().map_err(|error| error.to_string())?.is_file() {
                    return Err("review source was replaced by a nonregular file".into());
                }
                let mut contents = String::new();
                file.take(8 * 1024 * 1024 + 1).read_to_string(&mut contents)
                    .map_err(|error| error.to_string())?;
                if contents.len() > 8 * 1024 * 1024 {
                    return Err("review source exceeds 8 MiB".into());
                }
                contents
            };
            let snapshot = (contents.is_empty(), contents.lines().count());
            snapshots.insert(key, snapshot);
            Ok(snapshot)
        };
        for finding in &mut output.findings {
            if self.custom {
                // Custom prompts may embed source from outside the checkout.
                // With no host-derived scope, never invent validation receipts.
                continue;
            }
            let path = &finding.code_location.absolute_file_path;
            let relative = path
                .strip_prefix(&self.root)
                .or_else(|error| {
                    #[cfg(windows)]
                    {
                        if !path.is_absolute()
                            || path.components().any(|part| part == Component::ParentDir)
                        {
                            return Err(error);
                        }
                        // Accept a differently spelled workspace only when
                        // the filesystem confirms the same existing root.
                        let prefix = path
                            .components()
                            .take(self.root.components().count())
                            .collect::<PathBuf>();
                        if let (Ok(normalized_prefix), Ok(normalized_root)) = (
                            codex_utils_absolute_path::normalize_for_path_comparison(&prefix),
                            codex_utils_absolute_path::normalize_for_path_comparison(&self.root),
                        ) && normalized_prefix == normalized_root
                        {
                            return path.strip_prefix(prefix);
                        }
                    }
                    Err(error)
                })
                .map_err(|_| "review location is outside the requested workspace")?;
            if relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err("review location contains non-canonical path components".into());
            }
            let name = relative
                .to_str()
                .ok_or("review path is not UTF-8")?
                .replace('\\', "/");
            let range = &finding.code_location.line_range;
            let (mut deleted, mut changed) = changes.changed_ranges(&name)?;
            let overlaps = |changed: &[(u32, u32)]| changed.iter()
                .any(|(start, end)| range.start <= *end && range.end >= *start);
            let mut staged = false;
            if !overlaps(changed) && let Some(diff) = &staged_changes {
                (deleted, changed) = diff.changed_ranges(&name)?;
                staged = overlaps(changed);
            }
            let untracked = self.untracked.contains(&name);
            if !self.custom
                && !untracked
                && !overlaps(changed)
            {
                return Err(format!(
                    "review location does not overlap the requested change: {name}"
                ));
            }
            let revision = if deleted {
                let revision = if self
                    .comparison
                    .first()
                    .is_some_and(|command| command == "show")
                {
                    format!(
                        "{}^",
                        self.comparison.last().ok_or("missing review commit")?
                    )
                } else if self.staged_diff.is_some() {
                    self.head.clone().unwrap_or_default()
                } else {
                    self.comparison
                        .last()
                        .cloned()
                        .ok_or("missing review base")?
                };
                finding.body = format!(
                    "Base-side location at {revision} (not current workspace lines). {}",
                    finding.body
                );
                Some(revision)
            } else if staged {
                finding.body = format!("Index-side location (staged source, not workspace lines). {}", finding.body);
                Some(String::new())
            } else if self
                .comparison
                .first()
                .is_some_and(|command| command == "show")
            {
                Some(self.comparison.last().ok_or("missing review commit")?.clone())
            } else {
                None
            };
            let (empty, mut lines) = source_lines(path, &name, revision.as_deref())?;
            // A retained file can become empty through deletions alone. With
            // no surviving line, line 1 explicitly refers to its removed base.
            let empty_base = if historical {
                self.comparison.last().map(|commit| format!("{commit}^"))
            } else { self.head.clone() };
            if empty && range.start == 1 && range.end == 1
                && !untracked && let Some(base) = empty_base.as_deref() {
                (_, lines) = source_lines(path, &name, Some(base))?;
                finding.body = format!("Base-side location at {base} (file is now empty). {}", finding.body);
            }
            if range.end as usize > lines {
                return Err(format!("review location is beyond source EOF: {name}"));
            }
        }
        if self.custom {
            output.overall_explanation = format!(
                "Source validation: unverified custom scope; locations and freshness are not established by the host.\n\n{}",
                output.overall_explanation
            );
            return Ok(());
        }
        if historical {
            let revision = self.comparison.last().ok_or("missing review commit")?;
            output.overall_explanation = format!(
                "Source validation: unverified supporting evidence; locations checked against immutable commit {revision}, not current workspace lines.\n\n{}",
                output.overall_explanation,
            );
            return Ok(());
        }
        let head = self.head.as_deref().unwrap_or("unknown");
        output.overall_explanation = format!(
            "Source validation: unverified supporting evidence; workspace {}, admission HEAD {head}. This is not a current-source or repository-wide all-clear.\n\n{}",
            self.root.display(),
            output.overall_explanation,
        );
        Ok(())
    }
}

type ChangedReviewRanges = Result<(bool, Vec<(u32, u32)>), String>;

#[derive(Default)]
struct ReviewDiffIndex {
    files: HashMap<String, ChangedReviewRanges>,
    header_error: Option<String>,
}

impl ReviewDiffIndex {
    fn new(diff: &str) -> Self {
        let mut index = Self::default();
        let mut name = None;
        let mut deleted = false;
        // File labels precede a file's first hunk. Inside hunks, `--- ` and
        // `+++ ` prefixes are removed/added source lines, not labels.
        let mut in_header = false;
        let mut old_name = String::new();
        let mut ranges = Ok(Vec::new());
        for line in diff.lines() {
            if line.starts_with("diff --git ") {
                index.record(name.take(), deleted, std::mem::replace(&mut ranges, Ok(Vec::new())));
                deleted = false;
                in_header = true;
            }
            if line.starts_with("@@ ") {
                in_header = false;
            }
            if in_header && let Some(path) = line.strip_prefix("--- ") {
                match decode_diff_path(path) {
                    Ok(path) => old_name = path,
                    Err(error) => { index.header_error = Some(error); break; }
                }
            }
            if in_header && let Some(path) = line.strip_prefix("+++ ") {
                let new_name = match decode_diff_path(path) {
                    Ok(path) => path,
                    Err(error) => { index.header_error = Some(error); break; }
                };
                deleted = new_name == "/dev/null";
                name = if deleted {
                    old_name.strip_prefix("a/")
                } else {
                    new_name.strip_prefix("b/")
                }.map(str::to_owned);
            }
            if name.is_some() && line.starts_with("@@ ")
                && let Ok(current) = &mut ranges
            {
                match Self::hunk_range(line, deleted) {
                    Ok(range) => current.push(range),
                    Err(error) => ranges = Err(error),
                }
            }
        }
        index.record(name, deleted, ranges);
        index
    }

    fn record(&mut self, name: Option<String>, deleted: bool, ranges: Result<Vec<(u32, u32)>, String>) {
        let Some(name) = name else { return; };
        // The previous parser stopped after the first range-bearing file for
        // this path. A header-only entry can still be superseded by a later one.
        if self.files.get(&name).is_none_or(|entry| matches!(entry, Ok((_, ranges)) if ranges.is_empty())) {
            self.files.insert(name, ranges.map(|ranges| (deleted, ranges)));
        }
    }

    fn changed_ranges(&self, name: &str) -> Result<(bool, &[(u32, u32)]), String> {
        match self.files.get(name) {
            Some(Err(error)) => Err(error.clone()),
            Some(Ok((deleted, ranges))) if !ranges.is_empty() || self.header_error.is_none() => {
                Ok((*deleted, ranges))
            }
            _ => match &self.header_error {
                Some(error) => Err(error.clone()),
                None => Ok((false, &[])),
            },
        }
    }

    fn hunk_range(line: &str, deleted: bool) -> Result<(u32, u32), String> {
        let marker = if deleted { '-' } else { '+' };
        let field = line.split_whitespace().find(|field| field.starts_with(marker))
            .ok_or("invalid review diff hunk")?;
        let (start, count) = field[1..].split_once(',').unwrap_or((&field[1..], "1"));
        let start: u32 = start.parse().map_err(|_| "invalid review hunk start")?;
        let count: u32 = count.parse().map_err(|_| "invalid review hunk count")?;
        if count > 0 {
            Ok((start, start.checked_add(count - 1).ok_or("review hunk overflow")?))
        } else {
            // Anchor a deletion-only hunk on the preceding surviving line,
            // or line 1 for a beginning/whole-file deletion.
            Ok((start.max(1), start.max(1)))
        }
    }
}

fn decode_diff_path(path: &str) -> Result<String, String> {
    // Git ends a ---/+++ label that contains a space with one TAB; raw TABs
    // inside a name are always C-quoted.
    let path = path.strip_suffix('\t').unwrap_or(path);
    if path.len() >= 2 && path.starts_with('"') && path.ends_with('"') {
        let bytes = crate::unescape_c_bytes(&path[1..path.len() - 1]);
        String::from_utf8(bytes).map_err(|_| "review diff path is not UTF-8".into())
    } else {
        Ok(path.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::ReviewCodeLocation;
    use codex_protocol::protocol::ReviewFinding;
    use codex_protocol::protocol::ReviewLineRange;

    fn git(root: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new(crate::git_executable())
                .current_dir(root)
                .args(args)
                .output()
                .unwrap()
                .status
                .success(),
            "{args:?}"
        );
    }

    fn output(path: PathBuf, start: u32, end: u32) -> ReviewOutputEvent {
        ReviewOutputEvent {
            findings: vec![ReviewFinding {
                title: "[P2] Preserve the result".into(),
                body: "Input 1 now returns 0 instead of the original result.".into(),
                confidence_score: 0.8,
                priority: 2,
                code_location: ReviewCodeLocation {
                    absolute_file_path: path,
                    line_range: ReviewLineRange { start, end },
                },
            }],
            overall_correctness: "patch is incorrect".into(),
            overall_explanation: "The result is discarded.".into(),
            overall_confidence_score: 0.9,
        }
    }

    #[test]
    fn unborn_head_and_staged_edits_canceled_in_the_worktree_remain_reviewable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init"]);
        std::fs::write(root.join("source.rs"), "first\noriginal\n").unwrap();
        git(root, &["add", "."]);
        let unborn = ReviewSourceScope::capture(root, &ReviewTarget::UncommittedChanges).unwrap();
        unborn.validate(&mut output(unborn.root.join("source.rs"), 2, 2)).unwrap();
        git(root, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-m", "base"]);
        std::fs::write(root.join("source.rs"), "first\nbroken\n").unwrap();
        git(root, &["add", "."]);
        std::fs::write(root.join("source.rs"), "first\noriginal\n").unwrap();
        let staged = ReviewSourceScope::capture(root, &ReviewTarget::UncommittedChanges).unwrap();
        let mut result = output(staged.root.join("source.rs"), 2, 2);
        staged.validate(&mut result).unwrap();
        assert!(result.findings[0].body.starts_with("Index-side location"));
    }

    #[test]
    fn deletion_only_hunks_anchor_surviving_or_explicit_base_lines() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init"]);
        std::fs::write(root.join("source.rs"), "first\nremoved\nlast\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-m", "base"]);
        for contents in ["first\nlast\n", ""] {
            std::fs::write(root.join("source.rs"), contents).unwrap();
            let scope = ReviewSourceScope::capture(root, &ReviewTarget::UncommittedChanges).unwrap();
            scope.validate(&mut output(scope.root.join("source.rs"), 1, 1)).unwrap();
        }
    }

    #[test]
    fn merge_commit_scope_uses_the_first_parent_patch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init"]);
        std::fs::write(root.join("source.rs"), "first\noriginal\n").unwrap();
        git(root, &["add", "."]);
        let commit = |message| git(root, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-m", message]);
        commit("base");
        git(root, &["checkout", "-b", "side"]);
        std::fs::write(root.join("source.rs"), "first\nbroken\n").unwrap();
        git(root, &["add", "."]); commit("side");
        git(root, &["checkout", "-"]);
        std::fs::write(root.join("other.rs"), "other\n").unwrap();
        git(root, &["add", "."]); commit("main");
        git(root, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "merge", "--no-ff", "--no-edit", "side"]);
        let scope = ReviewSourceScope::capture(root, &ReviewTarget::Commit {sha:"HEAD".into(), title:None}).unwrap();
        scope.validate(&mut output(scope.root.join("source.rs"), 2, 2)).unwrap();
    }

    #[test]
    fn source_validation_rejects_off_diff_missing_and_beyond_eof_anchors_and_drift() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init"]);
        std::fs::write(
            root.join("new.rs"),
            "fn retained(value: i32) -> i32 {\n    value\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("correct.rs"), "fn correct() {}\n").unwrap();
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "base",
            ],
        );
        std::fs::write(
            root.join("new.rs"),
            "fn retained(value: i32) -> i32 {\n    0\n}\n",
        )
        .unwrap();
        let scope = ReviewSourceScope::capture(root, &ReviewTarget::UncommittedChanges).unwrap();
        for (path, start, end) in [
            ("new.rs", 2, 2),
            ("old.rs", 2, 2),
            ("correct.rs", 1, 1),
            ("new.rs", 2, u32::MAX),
            ("new.rs", 1, 1),
        ] {
            let mut result = output(scope.root.join(path), start, end);
            assert_eq!(
                scope.validate(&mut result).is_ok(),
                path == "new.rs" && start == 2 && end == 2,
                "{path}:{start}-{end}"
            );
        }
        std::fs::write(
            root.join("new.rs"),
            "fn retained(value: i32) -> i32 {\n    value\n}\n",
        )
        .unwrap();
        assert!(
            scope
                .validate(&mut output(scope.root.join("new.rs"), 2, 2))
                .is_err()
        );
        assert!(
            ReviewSourceScope::capture(
                root,
                &ReviewTarget::Commit {
                    sha: "missing-review-commit".into(),
                    title: None
                }
            )
            .is_err()
        );
    }

    #[test]
    fn historical_commit_locations_survive_unrelated_head_and_workspace_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init"]);
        std::fs::write(root.join("source.rs"), "fn result() {\n    before();\n}\n").unwrap();
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "base",
            ],
        );
        std::fs::write(root.join("source.rs"), "fn result() {\n    broken();\n}\n").unwrap();
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "reviewed",
            ],
        );
        let scope = ReviewSourceScope::capture(
            root,
            &ReviewTarget::Commit {
                sha: "HEAD".into(),
                title: None,
            },
        )
        .unwrap();
        std::fs::write(root.join("source.rs"), "fn repaired() {}\n").unwrap();
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "later",
            ],
        );
        let mut result = output(scope.root.join("source.rs"), 2, 2);
        scope.validate(&mut result).unwrap();
        assert!(result.overall_explanation.contains("immutable commit"));
        assert!(
            scope
                .validate(&mut output(scope.root.join("source.rs"), 2, 100))
                .is_err()
        );
    }

    #[test]
    fn deletion_uses_base_lines_and_rename_uses_new_path() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init"]);
        std::fs::write(
            temp.path().join("old.rs"),
            "fn retained() {\n    before();\n}\n",
        )
        .unwrap();
        git(temp.path(), &["add", "."]);
        git(
            temp.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "base",
            ],
        );
        std::fs::rename(temp.path().join("old.rs"), temp.path().join("new.rs")).unwrap();
        std::fs::write(
            temp.path().join("new.rs"),
            "fn retained() {\n    after();\n}\n",
        )
        .unwrap();
        let scope =
            ReviewSourceScope::capture(temp.path(), &ReviewTarget::UncommittedChanges).unwrap();
        let mut deleted = output(scope.root.join("old.rs"), 2, 2);
        scope.validate(&mut deleted).unwrap();
        assert!(
            deleted.findings[0]
                .body
                .starts_with("Base-side location at ")
        );
        let mut new = output(scope.root.join("new.rs"), 2, 2);
        scope.validate(&mut new).unwrap();
        assert!(!new.findings[0].body.contains("Base-side"));
    }

    #[test]
    fn spaced_labels_and_label_like_source_lines_keep_every_hunk() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init"]);
        std::fs::write(root.join("spaced name.rs"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "base",
            ],
        );
        // Git ends spaced ---/+++ labels with a TAB, and the added `++ two`
        // source line is emitted as `+++ two` inside the first hunk.
        std::fs::write(
            root.join("spaced name.rs"),
            "one\n++ two\nthree\nfour\nFIVE\n",
        )
        .unwrap();
        let scope = ReviewSourceScope::capture(root, &ReviewTarget::UncommittedChanges).unwrap();
        for line in [2, 5] {
            scope
                .validate(&mut output(scope.root.join("spaced name.rs"), line, line))
                .unwrap();
        }
        assert!(
            scope
                .validate(&mut output(scope.root.join("spaced name.rs"), 3, 3))
                .is_err()
        );
    }
}
