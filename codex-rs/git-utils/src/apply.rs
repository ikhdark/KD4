//! Helpers for applying unified diffs using the system `git` binary.
//!
//! The entry point is [`apply_git_patch`], which writes a diff to a temporary
//! file, shells out to `git apply` with the right flags, and then parses the
//! command’s output into structured details. Callers can opt into dry-run
//! mode via [`ApplyGitRequest::preflight`] and inspect the resulting paths to
//! learn what would change before applying for real.

use regex::Regex;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;

use crate::DISABLED_HOOKS_PATH;
use crate::operations::GitCommandControl;
use crate::operations::DEFAULT_GIT_OUTPUT_LIMIT;

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
    apply_git_patch_inner(req, &GitCommandControl::new())
}

/// Cancel contained Git work if the async caller is dropped. Cancellation is
/// not rollback: a final apply can already have changed some working files.
pub async fn apply_git_patch_async(req: ApplyGitRequest) -> io::Result<ApplyGitResult> {
    struct CancelOnDrop(GitCommandControl);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let control = GitCommandControl::new();
    let _cancellation = CancelOnDrop(control.clone());
    tokio::task::spawn_blocking(move || apply_git_patch_inner(&req, &control))
        .await.map_err(io::Error::other)?
}

fn apply_git_patch_inner(req: &ApplyGitRequest, control: &GitCommandControl) -> io::Result<ApplyGitResult> {
    let git_root = resolve_git_root(&req.cwd, control)?;

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
        prepare_temporary_index(&git_root, &patch_path, &req.diff, &cfg_parts, control)?;
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
        let (c_code, c_out, c_err) = run_git(&git_root, &cfg_parts, &check_args, Some(&git_env), control)?;
        let (applied_paths, skipped_paths, conflicted_paths) =
            parse_git_apply_output(&c_out, &c_err);
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
    let (code, stdout, stderr) = run_git(&git_root, &cfg_parts, &args, Some(&git_env), control)?;

    let (applied_paths, skipped_paths, conflicted_paths) =
        parse_git_apply_output(&stdout, &stderr);

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

fn resolve_git_root(cwd: &Path, control: &GitCommandControl) -> io::Result<PathBuf> {
    let mut command = std::process::Command::new(crate::git_executable());
    command.args(["rev-parse", "--show-toplevel"]).current_dir(cwd);
    let out = crate::operations::run_git_output(command, DEFAULT_GIT_OUTPUT_LIMIT, control)?;
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
    control: &GitCommandControl,
) -> io::Result<(i32, String, String)> {
    let args = args.iter().map(OsString::from).collect::<Vec<_>>();
    run_git_os(cwd, git_cfg, &args, env, control)
}

fn run_git_os(
    cwd: &Path,
    git_cfg: &[String],
    args: &[OsString],
    env: Option<&[(OsString, OsString)]>,
    control: &GitCommandControl,
) -> io::Result<(i32, String, String)> {
    let out = run_git_output_os(cwd, git_cfg, args, env, control)?;
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
    control: &GitCommandControl,
) -> io::Result<std::process::Output> {
    let mut cmd = std::process::Command::new(crate::git_executable());
    for p in git_cfg {
        cmd.arg(p);
    }
    for a in args {
        cmd.arg(a);
    }
    if let Some(env) = env {
        cmd.envs(env.iter().cloned());
    }
    cmd.current_dir(cwd);
    crate::operations::run_git_output(cmd, DEFAULT_GIT_OUTPUT_LIMIT, control)
}

fn prepare_temporary_index(
    git_root: &Path,
    patch_path: &Path,
    diff: &str,
    git_cfg: &[String],
    control: &GitCommandControl,
) -> io::Result<(tempfile::TempDir, PathBuf)> {
    let index_dir = tempfile::tempdir()?;
    let temporary_index = index_dir.path().join("index");
    let source_index = resolve_index_path(git_root, control)?;
    if source_index.is_file() {
        std::fs::copy(&source_index, &temporary_index)?;
    }

    let paths = paths_for_temporary_index(git_root, patch_path, diff, git_cfg, control)?;
    let git_env = [
        (
            OsString::from("GIT_INDEX_FILE"),
            temporary_index.clone().into_os_string(),
        ),
        (OsString::from("GIT_LITERAL_PATHSPECS"), OsString::from("1")),
    ];
    let tracked_paths = tracked_paths_in_index(git_root, git_cfg, &paths, &git_env, control)?;
    materialize_worktree_in_temporary_index(
        git_root,
        git_cfg,
        &paths,
        &tracked_paths,
        &git_env,
        index_dir.path(),
        control,
    )?;

    Ok((index_dir, temporary_index))
}

fn paths_for_temporary_index(
    git_root: &Path,
    patch_path: &Path,
    diff: &str,
    git_cfg: &[String],
    control: &GitCommandControl,
) -> io::Result<Vec<OsString>> {
    let args = vec![
        OsString::from("apply"),
        OsString::from("--numstat"),
        OsString::from("-z"),
        patch_path.as_os_str().to_os_string(),
    ];
    let output = run_git_output_os(git_root, git_cfg, &args, None, control)?;
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
    control: &GitCommandControl,
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
        let output = run_git_output_os(git_root, git_cfg, &args, Some(env), control)?;
        if !output.status.success() {
            return Err(git_output_error("diff worktree paths", &output));
        }
        if worktree_patch.len().checked_add(output.stdout.len())
            .is_none_or(|bytes| bytes > DEFAULT_GIT_OUTPUT_LIMIT)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                "Git worktree patch exceeds the supported capture limit"));
        }
        worktree_patch.extend(output.stdout);
    }

    let canonical_git_root = std::fs::canonicalize(git_root)?;
    let null_device = OsString::from("NUL");
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
        let output = run_git_output_os(git_root, git_cfg, &args, Some(env), control)?;
        if !output
            .status
            .code()
            .is_some_and(|code| code == 0 || code == 1)
        {
            return Err(git_output_error("diff untracked worktree path", &output));
        }
        if worktree_patch.len().checked_add(output.stdout.len())
            .is_none_or(|bytes| bytes > DEFAULT_GIT_OUTPUT_LIMIT)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                "Git worktree patch exceeds the supported capture limit"));
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
    let output = run_git_output_os(git_root, &internal_git_cfg, &args, Some(env), control)?;
    if !output.status.success() {
        return Err(git_output_error(
            "apply worktree state to temporary index",
            &output,
        ));
    }

    // `--refresh` always walks the whole index. Keep it quiet so unrelated local edits do not
    // fail the refresh, and pass no paths: positional paths are index updates, which reject
    // files the patch has not created yet.
    let refresh_args = [
        OsString::from("update-index"),
        OsString::from("-q"),
        OsString::from("--refresh"),
    ];
    let refresh = run_git_output_os(git_root, git_cfg, &refresh_args, Some(env), control)?;
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
    control: &GitCommandControl,
) -> io::Result<std::collections::BTreeSet<OsString>> {
    if paths.is_empty() {
        return Ok(std::collections::BTreeSet::new());
    }
    let mut cmd = std::process::Command::new(crate::git_executable());
    cmd.args(git_cfg).args(["ls-files", "-z", "--"]);
    cmd.args(paths)
        .envs(env.iter().cloned())
        .current_dir(git_root);
    let out = crate::operations::run_git_output(cmd, DEFAULT_GIT_OUTPUT_LIMIT, control)?;
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

fn resolve_index_path(git_root: &Path, control: &GitCommandControl) -> io::Result<PathBuf> {
    let mut command = std::process::Command::new(crate::git_executable());
    command.args(["rev-parse", "--git-path", "index"]).current_dir(git_root);
    let out = crate::operations::run_git_output(command, DEFAULT_GIT_OUTPUT_LIMIT, control)?;
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
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
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

/// Decode the contents of a Git C-quoted pathname, excluding its surrounding quotes.
pub fn unescape_c_bytes(input: &str) -> Vec<u8> {
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
    OsString::from(String::from_utf8_lossy(&bytes).into_owned())
}

// ============ Parser ported from VS Code (TS) ============

/// Parse `git apply` output into applied/skipped/conflicted path groupings.
pub fn parse_git_apply_output(
    stdout: &str,
    stderr: &str,
) -> (Vec<String>, Vec<String>, Vec<String>) {
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

    static APPLIED_CLEAN: LazyLock<Regex> =
        LazyLock::new(|| regex_ci("^Applied patch(?: to)?\\s+(?P<path>.+?)\\s+cleanly\\.?$"));
    static APPLIED_CONFLICTS: LazyLock<Regex> = LazyLock::new(|| {
        regex_ci("^Applied patch(?: to)?\\s+(?P<path>.+?)\\s+with conflicts\\.?$")
    });
    static APPLYING_WITH_REJECTS: LazyLock<Regex> = LazyLock::new(|| {
        regex_ci("^Applying patch\\s+(?P<path>.+?)\\s+with\\s+\\d+\\s+rejects?\\.{0,3}$")
    });
    static CHECKING_PATCH: LazyLock<Regex> =
        LazyLock::new(|| regex_ci("^Checking patch\\s+(?P<path>.+?)\\.\\.\\.$"));
    static UNMERGED_LINE: LazyLock<Regex> = LazyLock::new(|| regex_ci("^U\\s+(?P<path>.+)$"));
    static PATCH_FAILED: LazyLock<Regex> =
        LazyLock::new(|| regex_ci("^error:\\s+patch failed:\\s+(?P<path>.+?):\\d+$"));
    static DOES_NOT_APPLY: LazyLock<Regex> =
        LazyLock::new(|| regex_ci("^error:\\s+(?P<path>.+?):\\s+patch does not apply$"));
    static THREE_WAY_START: LazyLock<Regex> = LazyLock::new(|| {
        regex_ci("^(?:Performing three-way merge|Falling back to three-way merge)\\.\\.\\.$")
    });
    static THREE_WAY_FAILED: LazyLock<Regex> =
        LazyLock::new(|| regex_ci("^Failed to perform three-way merge\\.\\.\\.$"));
    static FALLBACK_DIRECT: LazyLock<Regex> =
        LazyLock::new(|| regex_ci("^Falling back to direct application\\.\\.\\.$"));
    static LACKS_BLOB: LazyLock<Regex> = LazyLock::new(|| {
        regex_ci(
            "^(?:error: )?repository lacks the necessary blob to (?:perform|fall back on) 3-?way merge\\.?$",
        )
    });
    static INDEX_MISMATCH: LazyLock<Regex> =
        LazyLock::new(|| regex_ci("^error:\\s+(?P<path>.+?):\\s+does not match index\\b"));
    static NOT_IN_INDEX: LazyLock<Regex> =
        LazyLock::new(|| regex_ci("^error:\\s+(?P<path>.+?):\\s+does not exist in index\\b"));
    static ALREADY_EXISTS_WT: LazyLock<Regex> = LazyLock::new(|| {
        regex_ci("^error:\\s+(?P<path>.+?)\\s+already exists in (?:the )?working directory\\b")
    });
    static FILE_EXISTS: LazyLock<Regex> =
        LazyLock::new(|| regex_ci("^error:\\s+patch failed:\\s+(?P<path>.+?)\\s+File exists"));
    static RENAMED_DELETED: LazyLock<Regex> = LazyLock::new(|| {
        regex_ci("^error:\\s+path\\s+(?P<path>.+?)\\s+has been renamed\\/deleted")
    });
    static CANNOT_APPLY_BINARY: LazyLock<Regex> = LazyLock::new(|| {
        regex_ci(
            "^error:\\s+cannot apply binary patch to\\s+['\\\"]?(?P<path>.+?)['\\\"]?\\s+without full index line$",
        )
    });
    static BINARY_DOES_NOT_APPLY: LazyLock<Regex> = LazyLock::new(|| {
        regex_ci("^error:\\s+binary patch does not apply to\\s+['\\\"]?(?P<path>.+?)['\\\"]?$")
    });
    static BINARY_INCORRECT_RESULT: LazyLock<Regex> = LazyLock::new(|| {
        regex_ci(
            "^error:\\s+binary patch to\\s+['\\\"]?(?P<path>.+?)['\\\"]?\\s+creates incorrect result\\b",
        )
    });
    static CANNOT_READ_CURRENT: LazyLock<Regex> = LazyLock::new(|| {
        regex_ci("^error:\\s+cannot read the current contents of\\s+['\\\"]?(?P<path>.+?)['\\\"]?$")
    });
    static SKIPPED_PATCH: LazyLock<Regex> =
        LazyLock::new(|| regex_ci("^Skipped patch\\s+['\\\"]?(?P<path>.+?)['\\\"]\\.$"));
    static CANNOT_MERGE_BINARY_WARN: LazyLock<Regex> = LazyLock::new(|| {
        regex_ci(
            "^warning:\\s*Cannot merge binary files:\\s+(?P<path>.+?)\\s+\\(ours\\s+vs\\.\\s+theirs\\)",
        )
    });

    for raw_line in stdout.lines().chain(stderr.lines()) {
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
            if let Some(m) = c.name("path")
                && let Some(p) = add(&mut applied, m.as_str())
            {
                conflicted.remove(&p);
                skipped.remove(&p);
                last_seen_path = Some(p);
            }
            continue;
        }
        if let Some(c) = APPLIED_CONFLICTS.captures(line) {
            if let Some(m) = c.name("path")
                && let Some(p) = add(&mut conflicted, m.as_str())
            {
                applied.remove(&p);
                skipped.remove(&p);
                last_seen_path = Some(p);
            }
            continue;
        }
        if let Some(c) = APPLYING_WITH_REJECTS.captures(line) {
            if let Some(m) = c.name("path")
                && let Some(p) = add(&mut conflicted, m.as_str())
            {
                applied.remove(&p);
                skipped.remove(&p);
                last_seen_path = Some(p);
            }
            continue;
        }

        // === “U <path>” after conflicts ===
        if let Some(c) = UNMERGED_LINE.captures(line) {
            if let Some(m) = c.name("path")
                && let Some(p) = add(&mut conflicted, m.as_str())
            {
                applied.remove(&p);
                skipped.remove(&p);
                last_seen_path = Some(p);
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
            if let Some(m) = c.name("path")
                && let Some(p) = add(&mut skipped, m.as_str())
            {
                applied.remove(&p);
                conflicted.remove(&p);
                last_seen_path = Some(p);
            }
            continue;
        }

        // === Warnings that imply conflicts ===
        if let Some(c) = CANNOT_MERGE_BINARY_WARN.captures(line) {
            if let Some(m) = c.name("path")
                && let Some(p) = add(&mut conflicted, m.as_str())
            {
                applied.remove(&p);
                skipped.remove(&p);
                last_seen_path = Some(p);
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

