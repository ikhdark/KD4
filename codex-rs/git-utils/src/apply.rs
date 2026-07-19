//! Helpers for applying unified diffs using the system `git` binary.
//!
//! The entry point is [`apply_git_patch`], which writes a diff to a temporary
//! file, shells out to `git apply` with the right flags, and then parses the
//! command’s output into structured details. Callers can opt into dry-run
//! mode via [`ApplyGitRequest::preflight`] and inspect the resulting paths to
//! learn what would change before applying for real.

use once_cell::sync::Lazy;
use regex::Regex;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const DISABLED_HOOKS_PATH: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// Parameters for invoking [`apply_git_patch`].
#[derive(Debug, Clone)]
pub struct ApplyGitRequest {
    pub cwd: PathBuf,
    pub diff: String,
    pub revert: bool,
    pub preflight: bool,
}

/// Result of running [`apply_git_patch`], including paths gleaned from stdout/stderr.
#[derive(Debug, Clone)]
pub struct ApplyGitResult {
    pub exit_code: i32,
    pub applied_paths: Vec<String>,
    pub skipped_paths: Vec<String>,
    pub conflicted_paths: Vec<String>,
    pub stdout: String,
    pub stderr: String,
    pub cmd_for_log: String,
}

/// Apply a unified diff to the target repository by shelling out to `git apply`.
///
/// Patch application uses a private index so successful changes remain unstaged and the caller's
/// existing staged/unstaged separation is preserved. When [`ApplyGitRequest::preflight`] is
/// `true`, the same three-way/index state is checked without modifying the working tree.
pub fn apply_git_patch(req: &ApplyGitRequest) -> io::Result<ApplyGitResult> {
    let git_root = resolve_git_root(&req.cwd)?;

    // Write unified diff into a temporary file
    let (tmpdir, patch_path) = write_temp_patch(&req.diff)?;
    // Keep tmpdir alive until function end to ensure the file exists
    let _guard = tmpdir;

    // Optional: additional git config via env knob (defaults OFF)
    let mut cfg_parts: Vec<String> = Vec::new();
    if let Ok(cfg) = std::env::var("CODEX_APPLY_GIT_CFG") {
        for pair in cfg.split(',') {
            let p = pair.trim();
            if p.is_empty() || !p.contains('=') {
                continue;
            }
            cfg_parts.push("-c".into());
            cfg_parts.push(p.to_string());
        }
    }
    cfg_parts.extend([
        "-c".to_string(),
        format!("core.hooksPath={DISABLED_HOOKS_PATH}"),
        "-c".to_string(),
        "core.fsmonitor=false".to_string(),
    ]);

    // `git apply --3way` implies `--index`. Use a private copy of the index that
    // reflects the current worktree for the touched paths so the real index is
    // never staged or otherwise reclassified by patch application.
    let (_index_dir, temporary_index) =
        prepare_temporary_index(&git_root, &patch_path, &req.diff, &cfg_parts)?;
    let git_env = [
        (
            OsString::from("GIT_INDEX_FILE"),
            temporary_index.into_os_string(),
        ),
        (OsString::from("GIT_LITERAL_PATHSPECS"), OsString::from("1")),
    ];

    // Build git args
    let mut args: Vec<String> = vec!["apply".into(), "--3way".into()];
    if req.revert {
        args.push("-R".into());
    }

    args.push(patch_path.to_string_lossy().to_string());

    // Optional preflight: dry-run only; do not modify working tree
    if req.preflight {
        let mut check_args = vec![
            "apply".to_string(),
            "--3way".to_string(),
            "--check".to_string(),
        ];
        if req.revert {
            check_args.push("-R".to_string());
        }
        check_args.push(patch_path.to_string_lossy().to_string());
        let rendered = render_command_for_log(&git_root, &cfg_parts, &check_args);
        let (c_code, c_out, c_err) = run_git(&git_root, &cfg_parts, &check_args, Some(&git_env))?;
        let (mut applied_paths, mut skipped_paths, mut conflicted_paths) =
            parse_git_apply_output(&c_out, &c_err);
        applied_paths.sort();
        applied_paths.dedup();
        skipped_paths.sort();
        skipped_paths.dedup();
        conflicted_paths.sort();
        conflicted_paths.dedup();
        // Git's `--3way --check` performs the in-core merge but skips the
        // write-out phase that normally turns recorded conflicts into a
        // nonzero exit. Preserve the real apply result contract explicitly.
        let exit_code = if c_code == 0 && !conflicted_paths.is_empty() {
            1
        } else {
            c_code
        };
        return Ok(ApplyGitResult {
            exit_code,
            applied_paths,
            skipped_paths,
            conflicted_paths,
            stdout: c_out,
            stderr: c_err,
            cmd_for_log: rendered,
        });
    }

    let cmd_for_log = render_command_for_log(&git_root, &cfg_parts, &args);
    let (code, stdout, stderr) = run_git(&git_root, &cfg_parts, &args, Some(&git_env))?;

    let (mut applied_paths, mut skipped_paths, mut conflicted_paths) =
        parse_git_apply_output(&stdout, &stderr);
    applied_paths.sort();
    applied_paths.dedup();
    skipped_paths.sort();
    skipped_paths.dedup();
    conflicted_paths.sort();
    conflicted_paths.dedup();

    Ok(ApplyGitResult {
        exit_code: code,
        applied_paths,
        skipped_paths,
        conflicted_paths,
        stdout,
        stderr,
        cmd_for_log,
    })
}

fn resolve_git_root(cwd: &Path) -> io::Result<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .current_dir(cwd)
        .output()?;
    let code = out.status.code().unwrap_or(-1);
    if code != 0 {
        return Err(io::Error::other(format!(
            "not a git repository (exit {}): {}",
            code,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(path_buf_from_git_bytes(trim_line_ending(&out.stdout)))
}

fn write_temp_patch(diff: &str) -> io::Result<(tempfile::TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("patch.diff");
    std::fs::write(&path, diff)?;
    Ok((dir, path))
}

fn run_git(
    cwd: &Path,
    git_cfg: &[String],
    args: &[String],
    env: Option<&[(OsString, OsString)]>,
) -> io::Result<(i32, String, String)> {
    let args = args.iter().map(OsString::from).collect::<Vec<_>>();
    run_git_os(cwd, git_cfg, &args, env)
}

fn run_git_os(
    cwd: &Path,
    git_cfg: &[String],
    args: &[OsString],
    env: Option<&[(OsString, OsString)]>,
) -> io::Result<(i32, String, String)> {
    let out = run_git_output_os(cwd, git_cfg, args, env)?;
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    Ok((code, stdout, stderr))
}

fn run_git_output_os(
    cwd: &Path,
    git_cfg: &[String],
    args: &[OsString],
    env: Option<&[(OsString, OsString)]>,
) -> io::Result<std::process::Output> {
    let mut cmd = std::process::Command::new("git");
    for p in git_cfg {
        cmd.arg(p);
    }
    for a in args {
        cmd.arg(a);
    }
    if let Some(env) = env {
        cmd.envs(env.iter().cloned());
    }
    cmd.current_dir(cwd).output()
}

fn prepare_temporary_index(
    git_root: &Path,
    patch_path: &Path,
    diff: &str,
    git_cfg: &[String],
) -> io::Result<(tempfile::TempDir, PathBuf)> {
    let index_dir = tempfile::tempdir()?;
    let temporary_index = index_dir.path().join("index");
    let source_index = resolve_index_path(git_root)?;
    if source_index.is_file() {
        std::fs::copy(&source_index, &temporary_index)?;
    }

    let paths = paths_for_temporary_index(git_root, patch_path, diff, git_cfg)?;
    let git_env = [
        (
            OsString::from("GIT_INDEX_FILE"),
            temporary_index.clone().into_os_string(),
        ),
        (OsString::from("GIT_LITERAL_PATHSPECS"), OsString::from("1")),
    ];
    let tracked_paths = tracked_paths_in_index(git_root, git_cfg, &paths, &git_env)?;
    materialize_worktree_in_temporary_index(
        git_root,
        git_cfg,
        &paths,
        &tracked_paths,
        &git_env,
        index_dir.path(),
    )?;

    Ok((index_dir, temporary_index))
}

fn paths_for_temporary_index(
    git_root: &Path,
    patch_path: &Path,
    diff: &str,
    git_cfg: &[String],
) -> io::Result<Vec<OsString>> {
    let args = vec![
        OsString::from("apply"),
        OsString::from("--numstat"),
        OsString::from("-z"),
        patch_path.as_os_str().to_os_string(),
    ];
    let output = run_git_output_os(git_root, git_cfg, &args, None)?;
    if output.status.success()
        && let Some(paths) = parse_numstat_paths(&output.stdout)
    {
        return Ok(paths);
    }

    // Preserve the existing error contract for malformed patches: let the real
    // `git apply` invocation return its exit status and diagnostics.
    Ok(extract_paths_from_patch_os(diff))
}

fn parse_numstat_paths(output: &[u8]) -> Option<Vec<OsString>> {
    let mut cursor = 0;
    let mut paths = std::collections::BTreeSet::new();
    while cursor < output.len() {
        let _added = take_delimited(output, &mut cursor, b'\t')?;
        let _deleted = take_delimited(output, &mut cursor, b'\t')?;
        let first_path = take_delimited(output, &mut cursor, 0)?;
        if first_path.is_empty() {
            let old_path = take_delimited(output, &mut cursor, 0)?;
            let new_path = take_delimited(output, &mut cursor, 0)?;
            insert_safe_git_path(&mut paths, old_path);
            insert_safe_git_path(&mut paths, new_path);
        } else {
            insert_safe_git_path(&mut paths, first_path);
        }
    }
    Some(paths.into_iter().collect())
}

fn take_delimited<'a>(input: &'a [u8], cursor: &mut usize, delimiter: u8) -> Option<&'a [u8]> {
    let start = *cursor;
    let relative_end = input
        .get(start..)?
        .iter()
        .position(|byte| *byte == delimiter)?;
    let end = start.checked_add(relative_end)?;
    *cursor = end.checked_add(1)?;
    Some(&input[start..end])
}

fn insert_safe_git_path(paths: &mut std::collections::BTreeSet<OsString>, raw_path: &[u8]) {
    let path = os_string_from_git_path_bytes(raw_path.to_vec());
    if is_safe_relative_git_path(&path) {
        paths.insert(path);
    }
}

fn is_safe_relative_git_path(path: &OsString) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn materialize_worktree_in_temporary_index(
    git_root: &Path,
    git_cfg: &[String],
    paths: &[OsString],
    tracked_paths: &std::collections::BTreeSet<OsString>,
    env: &[(OsString, OsString)],
    temp_dir: &Path,
) -> io::Result<()> {
    let mut worktree_patch = Vec::new();
    let tracked = paths
        .iter()
        .filter(|path| tracked_paths.contains(*path))
        .cloned()
        .collect::<Vec<_>>();
    if !tracked.is_empty() {
        let mut args = vec![
            OsString::from("diff"),
            OsString::from("--no-color"),
            OsString::from("--no-renames"),
            OsString::from("--binary"),
            OsString::from("--full-index"),
            OsString::from("--src-prefix=a/"),
            OsString::from("--dst-prefix=b/"),
            OsString::from("--no-textconv"),
            OsString::from("--no-ext-diff"),
            OsString::from("--"),
        ];
        args.extend(tracked);
        let output = run_git_output_os(git_root, git_cfg, &args, Some(env))?;
        if !output.status.success() {
            return Err(git_output_error("diff worktree paths", &output));
        }
        worktree_patch.extend(output.stdout);
    }

    let canonical_git_root = std::fs::canonicalize(git_root)?;
    let null_device = OsString::from(if cfg!(windows) { "NUL" } else { "/dev/null" });
    for path in paths
        .iter()
        .filter(|path| !tracked_paths.contains(*path))
        .filter(|path| existing_path_stays_within_root(git_root, &canonical_git_root, path))
    {
        let args = vec![
            OsString::from("diff"),
            OsString::from("--no-color"),
            OsString::from("--no-renames"),
            OsString::from("--binary"),
            OsString::from("--full-index"),
            OsString::from("--src-prefix=a/"),
            OsString::from("--dst-prefix=b/"),
            OsString::from("--no-textconv"),
            OsString::from("--no-ext-diff"),
            OsString::from("--no-index"),
            OsString::from("--"),
            null_device.clone(),
            path.clone(),
        ];
        let output = run_git_output_os(git_root, git_cfg, &args, Some(env))?;
        if !output
            .status
            .code()
            .is_some_and(|code| code == 0 || code == 1)
        {
            return Err(git_output_error("diff untracked worktree path", &output));
        }
        worktree_patch.extend(output.stdout);
    }

    if worktree_patch.is_empty() {
        return Ok(());
    }
    let worktree_patch_path = temp_dir.join("worktree.patch");
    std::fs::write(&worktree_patch_path, worktree_patch)?;
    let args = vec![
        OsString::from("apply"),
        OsString::from("--cached"),
        OsString::from("--whitespace=nowarn"),
        worktree_patch_path.into_os_string(),
    ];
    let mut internal_git_cfg = git_cfg.to_vec();
    internal_git_cfg.extend(["-c".to_string(), "apply.directory=".to_string()]);
    let output = run_git_output_os(git_root, &internal_git_cfg, &args, Some(env))?;
    if !output.status.success() {
        return Err(git_output_error(
            "apply worktree state to temporary index",
            &output,
        ));
    }

    let mut refresh_args = vec![
        OsString::from("update-index"),
        OsString::from("--refresh"),
        OsString::from("--"),
    ];
    refresh_args.extend(paths.iter().cloned());
    let refresh = run_git_output_os(git_root, git_cfg, &refresh_args, Some(env))?;
    if refresh.status.success() {
        Ok(())
    } else {
        Err(git_output_error("refresh temporary git index", &refresh))
    }
}

fn existing_path_stays_within_root(
    git_root: &Path,
    canonical_git_root: &Path,
    path: &OsString,
) -> bool {
    let joined = git_root.join(path);
    if std::fs::symlink_metadata(&joined).is_err() {
        return false;
    }
    let Some(parent) = joined.parent() else {
        return false;
    };
    std::fs::canonicalize(parent)
        .is_ok_and(|canonical_parent| canonical_parent.starts_with(canonical_git_root))
}

fn git_output_error(action: &str, output: &std::process::Output) -> io::Error {
    let code = output.status.code().unwrap_or(-1);
    io::Error::other(format!(
        "failed to {action} (exit {code}): {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn tracked_paths_in_index(
    git_root: &Path,
    git_cfg: &[String],
    paths: &[OsString],
    env: &[(OsString, OsString)],
) -> io::Result<std::collections::BTreeSet<OsString>> {
    if paths.is_empty() {
        return Ok(std::collections::BTreeSet::new());
    }
    let mut cmd = std::process::Command::new("git");
    cmd.args(git_cfg).args(["ls-files", "-z", "--"]);
    cmd.args(paths)
        .envs(env.iter().cloned())
        .current_dir(git_root);
    let out = cmd.output()?;
    let code = out.status.code().unwrap_or(-1);
    if code != 0 {
        return Err(io::Error::other(format!(
            "failed to inspect temporary git index (exit {code}): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| os_string_from_git_path_bytes(path.to_vec()))
        .collect())
}

fn resolve_index_path(git_root: &Path) -> io::Result<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "index"])
        .current_dir(git_root)
        .output()?;
    let code = out.status.code().unwrap_or(-1);
    if code != 0 {
        return Err(io::Error::other(format!(
            "failed to resolve git index (exit {code}): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let raw = trim_line_ending(&out.stdout);
    let path = path_buf_from_git_bytes(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        git_root.join(path)
    })
}

fn trim_line_ending(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn path_buf_from_git_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        PathBuf::from(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn quote_shell(s: &str) -> String {
    let simple = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.:/@%+".contains(c));
    if simple {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn render_command_for_log(cwd: &Path, git_cfg: &[String], args: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push("git".to_string());
    for a in git_cfg {
        parts.push(quote_shell(a));
    }
    for a in args {
        parts.push(quote_shell(a));
    }
    format!(
        "(cd {} && {})",
        quote_shell(&cwd.display().to_string()),
        parts.join(" ")
    )
}

/// Collect every path referenced by the diff headers inside `diff --git` sections.
pub fn extract_paths_from_patch(diff_text: &str) -> Vec<String> {
    extract_paths_from_patch_os(diff_text)
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn extract_paths_from_patch_os(diff_text: &str) -> Vec<OsString> {
    let mut set = std::collections::BTreeSet::new();
    for raw_line in diff_text.lines() {
        let line = raw_line.trim();
        let Some(rest) = line.strip_prefix("diff --git ") else {
            continue;
        };
        let Some((a, b)) = parse_diff_git_paths(rest) else {
            continue;
        };
        if let Some(a) = normalize_diff_path(&a, b"a/") {
            insert_safe_git_path(&mut set, &a);
        }
        if let Some(b) = normalize_diff_path(&b, b"b/") {
            insert_safe_git_path(&mut set, &b);
        }
    }
    set.into_iter().collect()
}

fn parse_diff_git_paths(line: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut chars = line.chars().peekable();
    let first = read_diff_git_token(&mut chars)?;
    let second = read_diff_git_token(&mut chars)?;
    Some((first, second))
}

fn read_diff_git_token(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<Vec<u8>> {
    while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
        chars.next();
    }
    let quote = match chars.peek().copied() {
        Some('"') | Some('\'') => chars.next(),
        _ => None,
    };
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                break;
            }
            if c == '\\' {
                out.push('\\');
                if let Some(next) = chars.next() {
                    out.push(next);
                }
                continue;
            }
        } else if c.is_whitespace() {
            break;
        }
        out.push(c);
    }
    if out.is_empty() && quote.is_none() {
        None
    } else {
        Some(match quote {
            Some(_) => unescape_c_bytes(&out),
            None => out.into_bytes(),
        })
    }
}

fn normalize_diff_path(raw: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
    if raw.is_empty() {
        return None;
    }
    if raw == b"/dev/null" || raw == [prefix, b"dev/null"].concat() {
        return None;
    }
    let path = raw.strip_prefix(prefix).unwrap_or(raw);
    if path.is_empty() {
        return None;
    }
    Some(path.to_vec())
}

fn unescape_c_string(input: &str) -> String {
    String::from_utf8_lossy(&unescape_c_bytes(input)).into_owned()
}

fn unescape_c_bytes(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        if index == bytes.len() {
            out.push(b'\\');
            break;
        }
        let next = bytes[index];
        index += 1;
        match next {
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'a' => out.push(0x07),
            b'v' => out.push(0x0b),
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            b'\'' => out.push(b'\''),
            b'0'..=b'7' => {
                let mut value = u16::from(next - b'0');
                for _ in 0..2 {
                    if index < bytes.len() && matches!(bytes[index], b'0'..=b'7') {
                        value = value * 8 + u16::from(bytes[index] - b'0');
                        index += 1;
                    } else {
                        break;
                    }
                }
                out.push(u8::try_from(value).unwrap_or(b'?'));
            }
            other => out.push(other),
        }
    }
    out
}

fn os_string_from_git_path_bytes(bytes: Vec<u8>) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        OsString::from_vec(bytes)
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Stage only the files that actually exist on disk for the given diff.
pub fn stage_paths(git_root: &Path, diff: &str) -> io::Result<()> {
    let paths = extract_paths_from_patch_os(diff);
    let mut existing: Vec<OsString> = Vec::new();
    for p in paths {
        let joined = git_root.join(&p);
        if std::fs::symlink_metadata(&joined).is_ok() {
            existing.push(p);
        }
    }
    if existing.is_empty() {
        return Ok(());
    }
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-c")
        .arg(format!("core.hooksPath={DISABLED_HOOKS_PATH}"))
        .args(["-c", "core.fsmonitor=false", "add"]);
    cmd.arg("--");
    for p in &existing {
        cmd.arg(p);
    }
    let out = cmd
        .env("GIT_LITERAL_PATHSPECS", "1")
        .current_dir(git_root)
        .output()?;
    let code = out.status.code().unwrap_or(-1);
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "git add failed (exit {code}): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

// ============ Parser ported from VS Code (TS) ============

/// Parse `git apply` output into applied/skipped/conflicted path groupings.
pub fn parse_git_apply_output(
    stdout: &str,
    stderr: &str,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let combined = [stdout, stderr]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<&str>>()
        .join("\n");

    let mut applied = std::collections::BTreeSet::new();
    let mut skipped = std::collections::BTreeSet::new();
    let mut conflicted = std::collections::BTreeSet::new();
    let mut last_seen_path: Option<String> = None;

    fn normalize_output_path(raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        let first = trimmed.chars().next().unwrap_or('\0');
        let last = trimmed.chars().last().unwrap_or('\0');
        let normalized = if (first == '"' || first == '\'') && last == first && trimmed.len() >= 2 {
            unescape_c_string(&trimmed[1..trimmed.len() - 1])
        } else {
            trimmed.to_string()
        };
        if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        }
    }

    fn add(set: &mut std::collections::BTreeSet<String>, raw: &str) -> Option<String> {
        let path = normalize_output_path(raw)?;
        set.insert(path.clone());
        Some(path)
    }

    static APPLIED_CLEAN: Lazy<Regex> =
        Lazy::new(|| regex_ci("^Applied patch(?: to)?\\s+(?P<path>.+?)\\s+cleanly\\.?$"));
    static APPLIED_CONFLICTS: Lazy<Regex> =
        Lazy::new(|| regex_ci("^Applied patch(?: to)?\\s+(?P<path>.+?)\\s+with conflicts\\.?$"));
    static APPLYING_WITH_REJECTS: Lazy<Regex> = Lazy::new(|| {
        regex_ci("^Applying patch\\s+(?P<path>.+?)\\s+with\\s+\\d+\\s+rejects?\\.{0,3}$")
    });
    static CHECKING_PATCH: Lazy<Regex> =
        Lazy::new(|| regex_ci("^Checking patch\\s+(?P<path>.+?)\\.\\.\\.$"));
    static UNMERGED_LINE: Lazy<Regex> = Lazy::new(|| regex_ci("^U\\s+(?P<path>.+)$"));
    static PATCH_FAILED: Lazy<Regex> =
        Lazy::new(|| regex_ci("^error:\\s+patch failed:\\s+(?P<path>.+?)(?::\\d+)?(?:\\s|$)"));
    static DOES_NOT_APPLY: Lazy<Regex> =
        Lazy::new(|| regex_ci("^error:\\s+(?P<path>.+?):\\s+patch does not apply$"));
    static THREE_WAY_START: Lazy<Regex> = Lazy::new(|| {
        regex_ci("^(?:Performing three-way merge|Falling back to three-way merge)\\.\\.\\.$")
    });
    static THREE_WAY_FAILED: Lazy<Regex> =
        Lazy::new(|| regex_ci("^Failed to perform three-way merge\\.\\.\\.$"));
    static FALLBACK_DIRECT: Lazy<Regex> =
        Lazy::new(|| regex_ci("^Falling back to direct application\\.\\.\\.$"));
    static LACKS_BLOB: Lazy<Regex> = Lazy::new(|| {
        regex_ci(
            "^(?:error: )?repository lacks the necessary blob to (?:perform|fall back on) 3-?way merge\\.?$",
        )
    });
    static INDEX_MISMATCH: Lazy<Regex> =
        Lazy::new(|| regex_ci("^error:\\s+(?P<path>.+?):\\s+does not match index\\b"));
    static NOT_IN_INDEX: Lazy<Regex> =
        Lazy::new(|| regex_ci("^error:\\s+(?P<path>.+?):\\s+does not exist in index\\b"));
    static ALREADY_EXISTS_WT: Lazy<Regex> = Lazy::new(|| {
        regex_ci("^error:\\s+(?P<path>.+?)\\s+already exists in (?:the )?working directory\\b")
    });
    static FILE_EXISTS: Lazy<Regex> =
        Lazy::new(|| regex_ci("^error:\\s+patch failed:\\s+(?P<path>.+?)\\s+File exists"));
    static RENAMED_DELETED: Lazy<Regex> =
        Lazy::new(|| regex_ci("^error:\\s+path\\s+(?P<path>.+?)\\s+has been renamed\\/deleted"));
    static CANNOT_APPLY_BINARY: Lazy<Regex> = Lazy::new(|| {
        regex_ci(
            "^error:\\s+cannot apply binary patch to\\s+['\\\"]?(?P<path>.+?)['\\\"]?\\s+without full index line$",
        )
    });
    static BINARY_DOES_NOT_APPLY: Lazy<Regex> = Lazy::new(|| {
        regex_ci("^error:\\s+binary patch does not apply to\\s+['\\\"]?(?P<path>.+?)['\\\"]?$")
    });
    static BINARY_INCORRECT_RESULT: Lazy<Regex> = Lazy::new(|| {
        regex_ci(
            "^error:\\s+binary patch to\\s+['\\\"]?(?P<path>.+?)['\\\"]?\\s+creates incorrect result\\b",
        )
    });
    static CANNOT_READ_CURRENT: Lazy<Regex> = Lazy::new(|| {
        regex_ci("^error:\\s+cannot read the current contents of\\s+['\\\"]?(?P<path>.+?)['\\\"]?$")
    });
    static SKIPPED_PATCH: Lazy<Regex> =
        Lazy::new(|| regex_ci("^Skipped patch\\s+['\\\"]?(?P<path>.+?)['\\\"]\\.$"));
    static CANNOT_MERGE_BINARY_WARN: Lazy<Regex> = Lazy::new(|| {
        regex_ci(
            "^warning:\\s*Cannot merge binary files:\\s+(?P<path>.+?)\\s+\\(ours\\s+vs\\.\\s+theirs\\)",
        )
    });

    for raw_line in combined.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        // === "Checking patch <path>..." tracking ===
        if let Some(c) = CHECKING_PATCH.captures(line) {
            if let Some(m) = c.name("path") {
                last_seen_path = normalize_output_path(m.as_str());
            }
            continue;
        }

        // === Status lines ===
        if let Some(c) = APPLIED_CLEAN.captures(line) {
            if let Some(m) = c.name("path") {
                if let Some(p) = add(&mut applied, m.as_str()) {
                    conflicted.remove(&p);
                    skipped.remove(&p);
                    last_seen_path = Some(p);
                }
            }
            continue;
        }
        if let Some(c) = APPLIED_CONFLICTS.captures(line) {
            if let Some(m) = c.name("path") {
                if let Some(p) = add(&mut conflicted, m.as_str()) {
                    applied.remove(&p);
                    skipped.remove(&p);
                    last_seen_path = Some(p);
                }
            }
            continue;
        }
        if let Some(c) = APPLYING_WITH_REJECTS.captures(line) {
            if let Some(m) = c.name("path") {
                if let Some(p) = add(&mut conflicted, m.as_str()) {
                    applied.remove(&p);
                    skipped.remove(&p);
                    last_seen_path = Some(p);
                }
            }
            continue;
        }

        // === “U <path>” after conflicts ===
        if let Some(c) = UNMERGED_LINE.captures(line) {
            if let Some(m) = c.name("path") {
                if let Some(p) = add(&mut conflicted, m.as_str()) {
                    applied.remove(&p);
                    skipped.remove(&p);
                    last_seen_path = Some(p);
                }
            }
            continue;
        }

        // === Early hints ===
        if PATCH_FAILED.is_match(line) || DOES_NOT_APPLY.is_match(line) {
            if let Some(c) = PATCH_FAILED
                .captures(line)
                .or_else(|| DOES_NOT_APPLY.captures(line))
                && let Some(m) = c.name("path")
            {
                last_seen_path = add(&mut skipped, m.as_str());
            }
            continue;
        }

        // === Ignore narration ===
        if THREE_WAY_START.is_match(line) || FALLBACK_DIRECT.is_match(line) {
            continue;
        }

        // === 3-way failed entirely; attribute to last_seen_path ===
        if THREE_WAY_FAILED.is_match(line) || LACKS_BLOB.is_match(line) {
            if let Some(p) = last_seen_path.clone() {
                skipped.insert(p.clone());
                applied.remove(&p);
                conflicted.remove(&p);
            }
            continue;
        }

        // === Skips / I/O problems ===
        if let Some(c) = INDEX_MISMATCH
            .captures(line)
            .or_else(|| NOT_IN_INDEX.captures(line))
            .or_else(|| ALREADY_EXISTS_WT.captures(line))
            .or_else(|| FILE_EXISTS.captures(line))
            .or_else(|| RENAMED_DELETED.captures(line))
            .or_else(|| CANNOT_APPLY_BINARY.captures(line))
            .or_else(|| BINARY_DOES_NOT_APPLY.captures(line))
            .or_else(|| BINARY_INCORRECT_RESULT.captures(line))
            .or_else(|| CANNOT_READ_CURRENT.captures(line))
            .or_else(|| SKIPPED_PATCH.captures(line))
        {
            if let Some(m) = c.name("path") {
                if let Some(p) = add(&mut skipped, m.as_str()) {
                    applied.remove(&p);
                    conflicted.remove(&p);
                    last_seen_path = Some(p);
                }
            }
            continue;
        }

        // === Warnings that imply conflicts ===
        if let Some(c) = CANNOT_MERGE_BINARY_WARN.captures(line) {
            if let Some(m) = c.name("path") {
                if let Some(p) = add(&mut conflicted, m.as_str()) {
                    applied.remove(&p);
                    skipped.remove(&p);
                    last_seen_path = Some(p);
                }
            }
            continue;
        }
    }

    // Final precedence: conflicts > applied > skipped
    for p in conflicted.iter() {
        applied.remove(p);
        skipped.remove(p);
    }
    for p in applied.iter() {
        skipped.remove(p);
    }

    (
        applied.into_iter().collect(),
        skipped.into_iter().collect(),
        conflicted.into_iter().collect(),
    )
}

fn regex_ci(pat: &str) -> Regex {
    Regex::new(&format!("(?i){pat}")).unwrap_or_else(|e| panic!("invalid regex: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::OnceLock;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn run(cwd: &Path, args: &[&str]) -> (i32, String, String) {
        let out = std::process::Command::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .output()
            .expect("spawn ok");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        // git init and minimal identity
        let _ = run(root, &["git", "init"]);
        let _ = run(root, &["git", "config", "user.email", "codex@example.com"]);
        let _ = run(root, &["git", "config", "user.name", "Codex"]);
        dir
    }

    fn read_file_normalized(path: &Path) -> String {
        std::fs::read_to_string(path)
            .expect("read file")
            .replace("\r\n", "\n")
    }

    #[test]
    fn extract_paths_handles_quoted_headers() {
        let diff = "diff --git \"a/hello world.txt\" \"b/hello world.txt\"\nnew file mode 100644\n--- /dev/null\n+++ b/hello world.txt\n@@ -0,0 +1 @@\n+hi\n";
        let paths = extract_paths_from_patch(diff);
        assert_eq!(paths, vec!["hello world.txt".to_string()]);
    }

    #[test]
    fn extract_paths_ignores_dev_null_header() {
        let diff = "diff --git a/dev/null b/ok.txt\nnew file mode 100644\n--- /dev/null\n+++ b/ok.txt\n@@ -0,0 +1 @@\n+hi\n";
        let paths = extract_paths_from_patch(diff);
        assert_eq!(paths, vec!["ok.txt".to_string()]);
    }

    #[test]
    fn extract_paths_unescapes_c_style_in_quoted_headers() {
        let diff = "diff --git \"a/hello\\tworld.txt\" \"b/hello\\tworld.txt\"\nnew file mode 100644\n--- /dev/null\n+++ b/hello\tworld.txt\n@@ -0,0 +1 @@\n+hi\n";
        let paths = extract_paths_from_patch(diff);
        assert_eq!(paths, vec!["hello\tworld.txt".to_string()]);
    }

    #[test]
    fn extract_paths_decodes_octal_utf8_as_one_path() {
        let diff = "diff --git \"a/\\303\\251.txt\" \"b/\\303\\251.txt\"\n--- \"a/\\303\\251.txt\"\n+++ \"b/\\303\\251.txt\"\n@@ -1 +1 @@\n-old\n+new\n";
        let paths = extract_paths_from_patch(diff);
        assert_eq!(paths, vec!["é.txt".to_string()]);
    }

    #[test]
    fn parse_output_unescapes_quoted_paths() {
        let stderr = "error: patch failed: \"hello\\tworld.txt\":1\n";
        let (applied, skipped, conflicted) = parse_git_apply_output("", stderr);
        assert_eq!(applied, Vec::<String>::new());
        assert_eq!(conflicted, Vec::<String>::new());
        assert_eq!(skipped, vec!["hello\tworld.txt".to_string()]);
    }

    #[test]
    fn parse_output_decodes_octal_utf8_paths() {
        let stderr = "error: patch failed: \"\\303\\251.txt\":1\n";
        let (applied, skipped, conflicted) = parse_git_apply_output("", stderr);
        assert_eq!(applied, Vec::<String>::new());
        assert_eq!(conflicted, Vec::<String>::new());
        assert_eq!(skipped, vec!["é.txt".to_string()]);
    }

    #[test]
    fn parse_output_attributes_failure_to_the_just_parsed_path() {
        let stderr = "Applied patch z.rs cleanly.\nApplied patch a.rs cleanly.\nFailed to perform three-way merge...\n";
        let (applied, skipped, conflicted) = parse_git_apply_output("", stderr);
        assert_eq!(applied, vec!["z.rs".to_string()]);
        assert_eq!(skipped, vec!["a.rs".to_string()]);
        assert_eq!(conflicted, Vec::<String>::new());
    }

    #[test]
    fn apply_preserves_existing_index_and_leaves_patch_unstaged() {
        let _g = env_lock().lock().unwrap();
        let repo = init_repo();
        let root = repo.path();

        std::fs::write(root.join("patch.txt"), "before\n").unwrap();
        std::fs::write(root.join("staged.txt"), "base staged\n").unwrap();
        std::fs::write(root.join("unstaged.txt"), "base unstaged\n").unwrap();
        let _ = run(root, &["git", "add", "."]);
        let _ = run(root, &["git", "commit", "-m", "seed"]);

        std::fs::write(root.join("patch.txt"), "after\n").unwrap();
        let (_, diff, _) = run(root, &["git", "diff", "--full-index", "--", "patch.txt"]);
        let _ = run(root, &["git", "restore", "patch.txt"]);

        std::fs::write(root.join("staged.txt"), "prepared commit\n").unwrap();
        let _ = run(root, &["git", "add", "staged.txt"]);
        std::fs::write(root.join("unstaged.txt"), "local work\n").unwrap();
        let (_, cached_before, _) = run(root, &["git", "diff", "--cached", "--binary"]);

        let result = apply_git_patch(&ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff,
            revert: false,
            preflight: false,
        })
        .expect("apply patch");

        assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
        let (_, cached_after, _) = run(root, &["git", "diff", "--cached", "--binary"]);
        assert_eq!(cached_after, cached_before);
        assert_eq!(
            read_file_normalized(&root.join("unstaged.txt")),
            "local work\n"
        );
        let (_, patch_staged, _) = run(root, &["git", "diff", "--cached", "--", "patch.txt"]);
        assert_eq!(patch_staged, "");
        assert_eq!(read_file_normalized(&root.join("patch.txt")), "after\n");
    }

    #[test]
    fn preflight_matches_clean_three_way_apply_without_mutating_worktree() {
        let _g = env_lock().lock().unwrap();
        let repo = init_repo();
        let root = repo.path();

        std::fs::write(root.join("file.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let _ = run(root, &["git", "add", "file.txt"]);
        let _ = run(root, &["git", "commit", "-m", "seed"]);
        std::fs::write(root.join("file.txt"), "one\nPATCH\nthree\nfour\nfive\n").unwrap();
        let (_, diff, _) = run(root, &["git", "diff", "--full-index", "--", "file.txt"]);
        let _ = run(root, &["git", "restore", "file.txt"]);
        std::fs::write(root.join("file.txt"), "one\ntwo\nthree\nfour\nLOCAL\n").unwrap();

        let preflight = apply_git_patch(&ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff: diff.clone(),
            revert: false,
            preflight: true,
        })
        .expect("preflight patch");
        assert_eq!(preflight.exit_code, 0, "stderr: {}", preflight.stderr);
        assert_eq!(
            read_file_normalized(&root.join("file.txt")),
            "one\ntwo\nthree\nfour\nLOCAL\n"
        );

        let applied = apply_git_patch(&ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff,
            revert: false,
            preflight: false,
        })
        .expect("apply patch");
        assert_eq!(applied.exit_code, 0, "stderr: {}", applied.stderr);
        assert_eq!(
            read_file_normalized(&root.join("file.txt")),
            "one\nPATCH\nthree\nfour\nLOCAL\n"
        );
    }

    #[test]
    fn preflight_reports_three_way_conflicts_as_failure() {
        let _g = env_lock().lock().unwrap();
        let repo = init_repo();
        let root = repo.path();

        std::fs::write(root.join("file.txt"), "before\n").unwrap();
        let _ = run(root, &["git", "add", "file.txt"]);
        let _ = run(root, &["git", "commit", "-m", "seed"]);
        std::fs::write(root.join("file.txt"), "patch\n").unwrap();
        let (_, diff, _) = run(root, &["git", "diff", "--full-index", "--", "file.txt"]);
        std::fs::write(root.join("file.txt"), "local\n").unwrap();

        let result = apply_git_patch(&ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff,
            revert: false,
            preflight: true,
        })
        .expect("preflight patch");

        assert_ne!(result.exit_code, 0);
        assert_eq!(result.conflicted_paths, vec!["file.txt".to_string()]);
        assert_eq!(read_file_normalized(&root.join("file.txt")), "local\n");
    }

    #[test]
    fn apply_add_success() {
        let _g = env_lock().lock().unwrap();
        let repo = init_repo();
        let root = repo.path();

        let diff = "diff --git a/hello.txt b/hello.txt\nnew file mode 100644\n--- /dev/null\n+++ b/hello.txt\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
        let req = ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff: diff.to_string(),
            revert: false,
            preflight: false,
        };
        let r = apply_git_patch(&req).expect("run apply");
        assert_eq!(r.exit_code, 0, "exit code 0");
        // File exists now
        assert!(root.join("hello.txt").exists());
    }

    #[test]
    fn apply_modify_conflict() {
        let _g = env_lock().lock().unwrap();
        let repo = init_repo();
        let root = repo.path();
        // seed file and commit
        std::fs::write(root.join("file.txt"), "line1\nline2\nline3\n").unwrap();
        let _ = run(root, &["git", "add", "file.txt"]);
        let _ = run(root, &["git", "commit", "-m", "seed"]);
        // local edit (unstaged)
        std::fs::write(root.join("file.txt"), "line1\nlocal2\nline3\n").unwrap();
        // patch wants to change the same line differently
        let diff = "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,3 @@\n line1\n-line2\n+remote2\n line3\n";
        let req = ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff: diff.to_string(),
            revert: false,
            preflight: false,
        };
        let r = apply_git_patch(&req).expect("run apply");
        assert_ne!(r.exit_code, 0, "non-zero exit on conflict");
    }

    #[test]
    fn apply_modify_skipped_missing_index() {
        let _g = env_lock().lock().unwrap();
        let repo = init_repo();
        let root = repo.path();
        // Try to modify a file that is not in the index
        let diff = "diff --git a/ghost.txt b/ghost.txt\n--- a/ghost.txt\n+++ b/ghost.txt\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let req = ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff: diff.to_string(),
            revert: false,
            preflight: false,
        };
        let r = apply_git_patch(&req).expect("run apply");
        assert_ne!(r.exit_code, 0, "non-zero exit on missing index");
    }

    #[test]
    fn apply_then_revert_success() {
        let _g = env_lock().lock().unwrap();
        let repo = init_repo();
        let root = repo.path();
        // Seed file and commit original content
        std::fs::write(root.join("file.txt"), "orig\n").unwrap();
        let _ = run(root, &["git", "add", "file.txt"]);
        let _ = run(root, &["git", "commit", "-m", "seed"]);

        // Forward patch: orig -> ORIG
        let diff = "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1,1 +1,1 @@\n-orig\n+ORIG\n";
        let apply_req = ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff: diff.to_string(),
            revert: false,
            preflight: false,
        };
        let res_apply = apply_git_patch(&apply_req).expect("apply ok");
        assert_eq!(res_apply.exit_code, 0, "forward apply succeeded");
        let after_apply = read_file_normalized(&root.join("file.txt"));
        assert_eq!(after_apply, "ORIG\n");

        // Revert patch: ORIG -> orig (stage paths first; engine handles it)
        let revert_req = ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff: diff.to_string(),
            revert: true,
            preflight: false,
        };
        let res_revert = apply_git_patch(&revert_req).expect("revert ok");
        assert_eq!(res_revert.exit_code, 0, "revert apply succeeded");
        let after_revert = read_file_normalized(&root.join("file.txt"));
        assert_eq!(after_revert, "orig\n");
    }

    #[test]
    fn revert_preflight_does_not_stage_index() {
        let _g = env_lock().lock().unwrap();
        let repo = init_repo();
        let root = repo.path();
        // Seed repo and apply forward patch so the working tree reflects the change.
        std::fs::write(root.join("file.txt"), "orig\n").unwrap();
        let _ = run(root, &["git", "add", "file.txt"]);
        let _ = run(root, &["git", "commit", "-m", "seed"]);

        let diff = "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1,1 +1,1 @@\n-orig\n+ORIG\n";
        let apply_req = ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff: diff.to_string(),
            revert: false,
            preflight: false,
        };
        let res_apply = apply_git_patch(&apply_req).expect("apply ok");
        assert_eq!(res_apply.exit_code, 0, "forward apply succeeded");
        let (commit_code, _, commit_err) = run(root, &["git", "commit", "-am", "apply change"]);
        assert_eq!(commit_code, 0, "commit applied change: {commit_err}");

        let (_code_before, staged_before, _stderr_before) =
            run(root, &["git", "diff", "--cached", "--name-only"]);

        let preflight_req = ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff: diff.to_string(),
            revert: true,
            preflight: true,
        };
        let res_preflight = apply_git_patch(&preflight_req).expect("preflight ok");
        assert_eq!(res_preflight.exit_code, 0, "revert preflight succeeded");
        let (_code_after, staged_after, _stderr_after) =
            run(root, &["git", "diff", "--cached", "--name-only"]);
        assert_eq!(
            staged_after.trim(),
            staged_before.trim(),
            "preflight should not stage new paths",
        );

        let after_preflight = read_file_normalized(&root.join("file.txt"));
        assert_eq!(after_preflight, "ORIG\n");
    }

    #[test]
    fn preflight_blocks_partial_changes() {
        let _g = env_lock().lock().unwrap();
        let repo = init_repo();
        let root = repo.path();
        // Build a multi-file diff: one valid add (ok.txt) and one invalid modify (ghost.txt)
        let diff = "diff --git a/ok.txt b/ok.txt\nnew file mode 100644\n--- /dev/null\n+++ b/ok.txt\n@@ -0,0 +1,2 @@\n+alpha\n+beta\n\n\
diff --git a/ghost.txt b/ghost.txt\n--- a/ghost.txt\n+++ b/ghost.txt\n@@ -1,1 +1,1 @@\n-old\n+new\n";

        // 1) With preflight enabled, nothing should be changed (even though ok.txt could be added)
        let req1 = ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff: diff.to_string(),
            revert: false,
            preflight: true,
        };
        let r1 = apply_git_patch(&req1).expect("preflight apply");
        assert_ne!(r1.exit_code, 0, "preflight reports failure");
        assert!(
            !root.join("ok.txt").exists(),
            "preflight must prevent adding ok.txt"
        );
        assert!(
            r1.cmd_for_log.contains("--check"),
            "preflight path recorded --check"
        );

        // 2) Without preflight, we should see no --check in the executed command
        let req2 = ApplyGitRequest {
            cwd: root.to_path_buf(),
            diff: diff.to_string(),
            revert: false,
            preflight: false,
        };
        let r2 = apply_git_patch(&req2).expect("direct apply");
        assert_ne!(r2.exit_code, 0, "apply is expected to fail overall");
        assert!(
            !r2.cmd_for_log.contains("--check"),
            "non-preflight path should not use --check"
        );
    }
}
