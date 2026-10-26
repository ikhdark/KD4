use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::ReadDenyMatcher;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

struct GlobScanPlan {
    root: PathBuf,
    max_depth: Option<usize>,
}

/// Resolve split filesystem `None` read entries into concrete Windows ACL targets.
///
/// Windows ACLs do not understand Codex filesystem glob patterns directly. Exact
/// unreadable roots can be passed through as-is, including paths that do not
/// exist yet. Glob entries are snapshot-expanded to the files/directories that
/// already exist under their literal scan root; future exact paths are handled
/// later by materializing them before the deny ACE is applied.
pub fn resolve_windows_deny_read_paths(
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    cwd: &AbsolutePathBuf,
) -> Result<Vec<AbsolutePathBuf>, String> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    for path in file_system_sandbox_policy.get_unreadable_roots_with_cwd(cwd.as_path()) {
        push_absolute_path(&mut paths, &mut seen, path.into_path_buf())?;
    }

    let unreadable_globs = file_system_sandbox_policy.get_unreadable_globs_with_cwd(cwd.as_path());
    if unreadable_globs.is_empty() {
        return Ok(paths);
    }

    let glob_policy = FileSystemSandboxPolicy::restricted(
        unreadable_globs
            .iter()
            .map(|pattern| FileSystemSandboxEntry {
                path: FileSystemPath::GlobPattern {
                    pattern: pattern.clone(),
                },
                access: FileSystemAccessMode::Deny,
            })
            .collect(),
    );
    let Some(matcher) = ReadDenyMatcher::try_new(&glob_policy, cwd.as_path())? else {
        return Ok(paths);
    };

    for pattern in unreadable_globs {
        let mut seen_scan_dirs = HashSet::new();
        let scan_plan = glob_scan_plan(&pattern, file_system_sandbox_policy.glob_scan_max_depth);
        collect_existing_glob_matches(
            &scan_plan.root,
            &matcher,
            &mut paths,
            &mut seen,
            &mut seen_scan_dirs,
            scan_plan.max_depth,
            /*depth*/ 0,
        )?;
    }

    Ok(paths)
}

fn collect_existing_glob_matches(
    path: &Path,
    matcher: &ReadDenyMatcher,
    paths: &mut Vec<AbsolutePathBuf>,
    seen_paths: &mut HashSet<PathBuf>,
    seen_scan_dirs: &mut HashSet<PathBuf>,
    max_depth: Option<usize>,
    depth: usize,
) -> Result<(), String> {
    let metadata = match path.metadata() {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(format!(
                "failed to inspect deny-read scan path {}: {err}",
                path.display()
            ));
        }
    };

    if matcher.is_read_denied(path) {
        push_absolute_path(paths, seen_paths, path.to_path_buf())?;
    }

    if !metadata.is_dir() {
        return Ok(());
    }

    if max_depth.is_some_and(|max_depth| depth >= max_depth) {
        return Ok(());
    }

    // Canonical ancestry keys keep recursive scans from following a symlink or
    // junction cycle forever while preserving the original matched path for the
    // ACL layer.
    let scan_key = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !seen_scan_dirs.insert(scan_key.clone()) {
        return Ok(());
    }

    let entries = std::fs::read_dir(path).map_err(|err| {
        format!(
            "failed to enumerate deny-read scan path {}: {err}",
            path.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "failed to read deny-read entry in {}: {err}",
                path.display()
            )
        })?;
        collect_existing_glob_matches(
            &entry.path(),
            matcher,
            paths,
            seen_paths,
            seen_scan_dirs,
            max_depth,
            depth + 1,
        )?;
    }
    seen_scan_dirs.remove(&scan_key);

    Ok(())
}

fn push_absolute_path(
    paths: &mut Vec<AbsolutePathBuf>,
    seen: &mut HashSet<PathBuf>,
    path: PathBuf,
) -> Result<(), String> {
    let absolute_path = AbsolutePathBuf::from_absolute_path(dunce::simplified(&path))
        .map_err(|err| err.to_string())?;
    if seen.insert(absolute_path.to_path_buf()) {
        paths.push(absolute_path);
    }
    Ok(())
}

fn glob_scan_plan(pattern: &str, configured_max_depth: Option<usize>) -> GlobScanPlan {
    // Start scanning at the deepest literal directory prefix before the first
    // glob metacharacter. For example, `C:\repo\**\*.env` only scans `C:\repo`
    // instead of the current directory or drive root. `{` starts a globset
    // alternation, so `C:\repo\{a,b}\*` must scan `C:\repo` as well.
    let first_glob = pattern
        .char_indices()
        .find(|(_, ch)| matches!(ch, '*' | '?' | '[' | '{'))
        .map(|(index, _)| index)
        .unwrap_or(pattern.len());
    let literal_prefix = &pattern[..first_glob];
    let Some(separator_index) = literal_prefix.rfind(['/', '\\']) else {
        return GlobScanPlan {
            root: PathBuf::from("."),
            max_depth: effective_glob_scan_max_depth(pattern, configured_max_depth),
        };
    };
    let pattern_suffix = &pattern[separator_index + 1..];
    let is_drive_root_separator = separator_index > 0
        && literal_prefix
            .as_bytes()
            .get(separator_index - 1)
            .is_some_and(|ch| *ch == b':');
    if separator_index == 0 || is_drive_root_separator {
        return GlobScanPlan {
            root: PathBuf::from(&literal_prefix[..=separator_index]),
            max_depth: effective_glob_scan_max_depth(pattern_suffix, configured_max_depth),
        };
    }
    GlobScanPlan {
        root: PathBuf::from(literal_prefix[..separator_index].to_string()),
        max_depth: effective_glob_scan_max_depth(pattern_suffix, configured_max_depth),
    }
}

fn effective_glob_scan_max_depth(
    pattern_suffix: &str,
    configured_max_depth: Option<usize>,
) -> Option<usize> {
    let components = pattern_suffix
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    if pattern_suffix.contains("**") {
        return configured_max_depth;
    }
    Some(configured_max_depth.map_or(components.len(), |max_depth| {
        max_depth.min(components.len())
    }))
}

