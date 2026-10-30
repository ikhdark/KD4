//! Gitignore parsing without host-side I/O: every read uses the selected filesystem.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use codex_utils_path_uri::PathUri;
use ignore::gitignore::Gitignore;
use ignore::gitignore::GitignoreBuilder;

use crate::ExecutorFileSystem;
use crate::FileSystemSandboxContext;

const MAX_ANCESTORS: usize = 64;
const MAX_IGNORE_FILE_BYTES: usize = 256 * 1024;
const MAX_IGNORE_BYTES: usize = 4 * 1024 * 1024;

pub(super) struct WalkIgnoreRules {
    matcher: Gitignore,
    parent: Option<Arc<Self>>,
}

impl WalkIgnoreRules {
    /// The nearest rule file with a match decides, as in Git. Only the entry
    /// itself is matched: the walker prunes ignored directories below the root
    /// and never re-evaluates directories above it, so an explicitly selected
    /// root inside an ignored directory is listed (as with rg/fd), and a nested
    /// negated directory pattern cannot re-include a parent file's matches.
    pub(super) fn is_ignored(&self, path: &PathUri, is_directory: bool) -> bool {
        let native = path.to_path_buf();
        let mut rules = Some(self);
        while let Some(current) = rules {
            let matched = current.matcher.matched(&native, is_directory);
            if !matched.is_none() {
                return matched.is_ignore();
            }
            rules = current.parent.as_deref();
        }
        false
    }
}

pub(super) struct WalkGitignore {
    /// Directories inside a repository; outside one nothing is recorded.
    directories: HashMap<PathUri, Option<Arc<WalkIgnoreRules>>>,
    /// Whether the walk root is inside a repository. As in Git and ripgrep,
    /// outside one rule files apply only from a nested repository down.
    in_repository: bool,
    remaining_bytes: usize,
    /// Rule-file defects Git tolerates, reported once instead of failing the walk.
    problems: Vec<(PathUri, String)>,
}

fn ignore_error(path: &PathUri, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!(
        "cannot apply gitignore rules at {path}: {error}; retry without gitignore filtering for an unfiltered listing"
    ))
}

/// Contention with a concurrent native patch capture is transient: keep it
/// retryable instead of advising an unfiltered listing.
fn probe_error(path: &PathUri, error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::WouldBlock {
        return io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("cannot apply gitignore rules at {path} yet: {error}; retry the same listing"),
        );
    }
    ignore_error(path, error)
}

impl WalkGitignore {
    pub(super) async fn new<F: ExecutorFileSystem + ?Sized>(
        file_system: &F,
        root: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> io::Result<Self> {
        let mut ancestors = Vec::new();
        let mut in_repository = false;
        for directory in root.ancestors() {
            if ancestors.len() == MAX_ANCESTORS {
                return Err(ignore_error(root, "ancestor limit exceeded"));
            }
            let marker = directory
                .join(".git")
                .map_err(|error| ignore_error(&directory, error))?;
            ancestors.push(directory);
            match file_system.get_metadata(&marker, sandbox).await {
                Ok(_) => {
                    in_repository = true;
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(probe_error(&marker, error)),
            }
        }
        let mut state = Self {
            directories: HashMap::new(),
            in_repository,
            remaining_bytes: MAX_IGNORE_BYTES,
            problems: Vec::new(),
        };
        // The outermost ancestor is the nearest repository; nothing above it is
        // read, and outside a repository no ancestor is.
        if in_repository {
            for directory in ancestors.into_iter().rev() {
                state
                    .enter_directory(
                        file_system,
                        &directory,
                        /*repository_root*/ false,
                        /*ignore_file_possible*/ true,
                        sandbox,
                    )
                    .await?;
            }
        }
        Ok(state)
    }

    /// Tolerated rule-file defects observed since the previous call.
    pub(super) fn take_problems(&mut self) -> Vec<(PathUri, String)> {
        std::mem::take(&mut self.problems)
    }

    /// `repository_root` marks a nested repository (`.git` in its listing):
    /// as in Git and ripgrep, rules from outside it do not apply inside it.
    /// `ignore_file_possible` is false only when a complete listing has no
    /// `.gitignore`, leaving nothing to read.
    pub(super) async fn enter_directory<F: ExecutorFileSystem + ?Sized>(
        &mut self,
        file_system: &F,
        directory: &PathUri,
        repository_root: bool,
        ignore_file_possible: bool,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> io::Result<Option<Arc<WalkIgnoreRules>>> {
        if let Some(rules) = self.directories.get(directory) {
            return Ok(rules.clone());
        }
        let parent = if repository_root {
            None
        } else {
            match directory.parent().and_then(|path| self.directories.get(&path)) {
                Some(rules) => rules.clone(),
                // Outside every repository no rule file is read or applied.
                None if !self.in_repository => return Ok(None),
                None => None,
            }
        };
        let path = directory
            .join(".gitignore")
            .map_err(|error| ignore_error(directory, error))?;
        let bytes = if ignore_file_possible {
            let limit = MAX_IGNORE_FILE_BYTES.min(self.remaining_bytes);
            match file_system
                .read_file_bounded_confined(&path, directory, limit, sandbox)
                .await
            {
                Ok(Some(bytes)) => Some(bytes),
                Ok(None) => {
                    return Err(ignore_error(
                        &path,
                        "ignore-file byte budget exceeded or file changed while reading",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(probe_error(&path, error)),
            }
        } else {
            None
        };
        let rules = if let Some(bytes) = bytes {
            self.remaining_bytes -= bytes.len();
            match self.parse_rules(directory, &path, &bytes) {
                Some(matcher) => Some(Arc::new(WalkIgnoreRules { matcher, parent })),
                None => parent,
            }
        } else {
            parent
        };
        self.directories.insert(directory.clone(), rules.clone());
        Ok(rules)
    }

    /// Git reads rule files as bytes: UTF-16 files are inert, undecodable bytes
    /// never match a Windows name, and an invalid pattern never matches. Mirror
    /// that instead of failing the listing, but report what was not applied.
    fn parse_rules(
        &mut self,
        directory: &PathUri,
        path: &PathUri,
        bytes: &[u8],
    ) -> Option<Gitignore> {
        if bytes.starts_with(b"\xFF\xFE") || bytes.starts_with(b"\xFE\xFF") {
            self.problems.push((
                path.clone(),
                "UTF-16 ignore file not applied: Git reads ignore files as UTF-8, so its rules take no effect".into(),
            ));
            return None;
        }
        let text = String::from_utf8_lossy(bytes);
        let source = path.to_path_buf();
        let mut builder = GitignoreBuilder::new(directory.to_path_buf());
        // Git for Windows initializes repositories with core.ignorecase=true.
        // This setter cannot fail; its Result is an API artifact.
        let _ = builder.case_insensitive(true);
        let mut invalid_lines = 0usize;
        let mut first_invalid = None;
        for (index, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
            if let Err(error) = builder.add_line(Some(source.clone()), line) {
                invalid_lines += 1;
                if first_invalid.is_none() {
                    first_invalid = Some(format!("line {}: {error}", index + 1));
                }
            }
        }
        if let Some(first_invalid) = first_invalid {
            self.problems.push((
                path.clone(),
                format!("{invalid_lines} invalid gitignore line(s) not applied; {first_invalid}"),
            ));
        }
        match builder.build() {
            Ok(matcher) => (!matcher.is_empty()).then_some(matcher),
            Err(error) => {
                self.problems
                    .push((path.clone(), format!("gitignore rules not applied: {error}")));
                None
            }
        }
    }
}
