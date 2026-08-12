//! Utility to compute the current Git diff for the working directory.
//!
//! The implementation mirrors the behaviour of the TypeScript version in
//! `codex-cli`: it returns the diff for tracked changes as well as any
//! untracked files. When the current directory is not inside a Git
//! repository, the function returns `Ok((false, String::new()))`.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(unix)]
use std::ffi::OsString;

use crate::workspace_command::WorkspaceCommand;
use crate::workspace_command::WorkspaceCommandExecutor;
use crate::workspace_command::WorkspaceCommandOutput;
use codex_git_utils::FsmonitorOverride;
use codex_git_utils::FsmonitorProbeRunner;
use codex_git_utils::detect_fsmonitor_override;
use diffy::DiffOptions;
use sha1::Digest;
use sha1::Sha1;

const DIFF_COMMAND_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 30);
const DISABLE_HOOKS_CONFIG: &str = if cfg!(windows) {
    "core.hooksPath=NUL"
} else {
    "core.hooksPath=/dev/null"
};
const EXECUTABLE_FILTER_CONFIG_PATTERN: &str = r"^filter\..*\.(clean|process)$";
const MAX_UNTRACKED_FILE_DIFFS: usize = 50;
const MAX_UNTRACKED_FILE_BYTES: u64 = 1024 * 1024;
const MAX_UNTRACKED_TOTAL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_OMITTED_UNTRACKED_PATHS: usize = 50;
const MAX_OMITTED_UNTRACKED_PATH_CHARS: usize = 200;

// `/diff` may execute Git through a remote workspace, so git-utils owns the
// probe policy while this adapter keeps command execution in the TUI layer.
// WorkspaceCommand bounds each call; `/diff` has no aggregate command deadline.
struct WorkspaceFsmonitorProbeRunner<'a> {
    runner: &'a dyn WorkspaceCommandExecutor,
    cwd: &'a Path,
}

impl FsmonitorProbeRunner for WorkspaceFsmonitorProbeRunner<'_> {
    async fn run_probe(&mut self, args: &[&str]) -> Option<Vec<u8>> {
        let argv = ["git"].into_iter().chain(args.iter().copied());
        let command = WorkspaceCommand::new(argv).cwd(self.cwd.to_path_buf());
        match self.runner.run(command).await {
            Ok(output) if output.success() => Some(output.stdout.into_bytes()),
            _ => None,
        }
    }
}

/// Return value of [`get_git_diff`].
///
/// * `bool` – Whether the current working directory is inside a Git repo.
/// * `String` – The concatenated diff (may be empty).
pub(crate) async fn get_git_diff(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Result<(bool, String), String> {
    // First check if we are inside a Git repository.
    if !inside_git_repo(runner, cwd).await? {
        return Ok((false, String::new()));
    }

    // Probe once per `/diff` and reuse the result for all subsequent Git commands.
    let mut probe_runner = WorkspaceFsmonitorProbeRunner { runner, cwd };
    let fsmonitor = detect_fsmonitor_override(&mut probe_runner).await;

    // Keep `/diff` informational: repository configuration must not select executable diff helpers.
    let diff_config_overrides = diff_filter_config_overrides(runner, cwd, fsmonitor).await?;
    let (tracked_diff_res, untracked_output_res) = tokio::join!(
        run_git_capture_diff(
            runner,
            cwd,
            fsmonitor,
            &diff_config_overrides,
            &[
                "diff",
                "--no-textconv",
                "--no-ext-diff",
                "--submodule=short",
                "--ignore-submodules=dirty",
                "--color",
            ]
        ),
        run_git_capture_stdout(
            runner,
            cwd,
            fsmonitor,
            &[
                "-c",
                "core.quotePath=true",
                "ls-files",
                "--others",
                "--exclude-standard",
            ]
        ),
    );
    let tracked_diff = tracked_diff_res?;
    let untracked_output = untracked_output_res?;

    let mut untracked_diff = String::new();
    let null_device: &Path = if cfg!(windows) {
        Path::new("NUL")
    } else {
        Path::new("/dev/null")
    };

    let null_path = null_device.to_str().unwrap_or("/dev/null");
    let untracked_files = parse_untracked_files(&untracked_output)?;
    let (files_to_diff, omitted_files) =
        untracked_files.split_at(untracked_files.len().min(MAX_UNTRACKED_FILE_DIFFS));
    let fallback_deadline = tokio::time::Instant::now() + DIFF_COMMAND_TIMEOUT;
    let mut untracked_budget_used = 0_u64;
    for file in files_to_diff {
        match render_local_untracked_file(
            cwd,
            file,
            MAX_UNTRACKED_TOTAL_BYTES.saturating_sub(untracked_budget_used),
        ) {
            Ok(Some((diff, bytes))) => {
                untracked_budget_used = untracked_budget_used.saturating_add(bytes);
                untracked_diff.push_str(&diff);
                continue;
            }
            Ok(None) => {
                untracked_diff.push_str(&format!(
                    "# Untracked file diff omitted because it exceeds the bounded read budget: {}\n",
                    escaped_path_for_notice(file)
                ));
                continue;
            }
            Err(_) => {}
        }

        let Some(file_arg) = file.to_str() else {
            untracked_diff.push_str(&format!(
                "# Remote untracked file diff omitted because its path is not UTF-8: {}\n",
                escaped_path_for_notice(file)
            ));
            continue;
        };

        let remaining_response_budget =
            MAX_UNTRACKED_TOTAL_BYTES.saturating_sub(untracked_budget_used);
        if remaining_response_budget <= 1 {
            untracked_diff.push_str(
                "# Remaining untracked file diffs omitted after bounded response budget\n",
            );
            break;
        }
        // App-server does not report whether output hit its cap. Reserve one sentinel byte so a
        // response larger than `complete_output_budget` can be discarded as a whole instead of
        // presenting a silently truncated diff hunk.
        let complete_output_budget =
            MAX_UNTRACKED_FILE_BYTES.min(remaining_response_budget.saturating_sub(1)) as usize;

        // Remote workspace runners do not expose a file-read API. Preserve the existing
        // executor-backed Git path when the file is not locally readable rather than widening
        // that contract or making remote `/diff` incomplete.
        let args = [
            "diff",
            "--no-textconv",
            "--no-ext-diff",
            "--submodule=short",
            "--ignore-submodules=dirty",
            "--color",
            "--no-index",
            "--",
            null_path,
            file_arg,
        ];
        let remaining = fallback_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            untracked_diff.push_str("# Remaining untracked file diffs omitted after deadline\n");
            break;
        }
        let diff = match tokio::time::timeout(
            remaining,
            run_git_capture_diff_bounded(
                runner,
                cwd,
                fsmonitor,
                &diff_config_overrides,
                &args,
                complete_output_budget,
            ),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                untracked_diff
                    .push_str("# Remaining untracked file diffs omitted after deadline\n");
                break;
            }
        };
        untracked_budget_used = untracked_budget_used.saturating_add(diff.captured_bytes as u64);
        if let Some(output) = diff.output {
            untracked_diff.push_str(&output);
        } else {
            untracked_diff.push_str(&format!(
                "# Remote untracked file diff omitted because its complete output exceeds the bounded response budget: {}\n",
                escaped_path_for_notice(file)
            ));
        }
    }
    append_omitted_untracked_diff_notice(&mut untracked_diff, omitted_files);

    Ok((true, format!("{tracked_diff}{untracked_diff}")))
}

fn render_local_untracked_file(
    cwd: &Path,
    file: &Path,
    remaining_budget: u64,
) -> std::io::Result<Option<(String, u64)>> {
    let root = cwd.canonicalize()?;
    let path = root.join(file);
    let metadata = fs::symlink_metadata(&path)?;
    let (contents, mode) = if metadata.file_type().is_symlink() {
        (
            fs::read_link(&path)?
                .to_string_lossy()
                .into_owned()
                .into_bytes(),
            "120000",
        )
    } else if metadata.is_file() {
        let limit = MAX_UNTRACKED_FILE_BYTES.min(remaining_budget);
        let opened = codex_file_system::open_confined_file(&root, &path)?;
        let opened_metadata = opened.metadata()?;
        if opened_metadata.len() > limit {
            return Ok(None);
        }
        let mut contents = Vec::with_capacity(opened_metadata.len() as usize);
        opened
            .take(limit.saturating_add(1))
            .read_to_end(&mut contents)?;
        if contents.len() as u64 > limit {
            return Ok(None);
        }
        (contents, untracked_file_mode(&opened_metadata))
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "untracked path is not a file or symlink",
        ));
    };

    let bytes = contents.len() as u64;
    Ok(Some((
        render_untracked_new_file(&file.to_string_lossy(), mode, &contents),
        bytes,
    )))
}

#[cfg(unix)]
fn untracked_file_mode(metadata: &fs::Metadata) -> &'static str {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o111 == 0 {
        "100644"
    } else {
        "100755"
    }
}

#[cfg(not(unix))]
fn untracked_file_mode(_metadata: &fs::Metadata) -> &'static str {
    "100644"
}

fn render_untracked_new_file(file: &str, mode: &str, contents: &[u8]) -> String {
    let old_path = "/dev/null";
    let new_path = format!("b/{file}");
    let mut output = String::new();
    output.push_str("\x1b[1mdiff --git ");
    output.push_str(&quote_diff_path(&format!("a/{file}")));
    output.push(' ');
    output.push_str(&quote_diff_path(&new_path));
    output.push_str("\x1b[m\n");
    output.push_str(&format!("\x1b[1mnew file mode {mode}\x1b[m\n"));
    output.push_str(&format!(
        "\x1b[1mindex 0000000..{}\x1b[m\n",
        git_blob_abbreviation(contents)
    ));

    if contents.is_empty() {
        return output;
    }
    if contents.contains(&0) {
        output.push_str("Binary files /dev/null and ");
        output.push_str(&quote_diff_path(&new_path));
        output.push_str(" differ\n");
        return output;
    }

    let mut options = DiffOptions::new();
    options
        .set_original_filename(old_path)
        .set_modified_filename(new_path.clone());
    let contents = String::from_utf8_lossy(contents);
    let patch = options.create_patch("", &contents).to_string();
    let mut lines = patch.lines();
    let _ = lines.next();
    let _ = lines.next();

    output.push_str("\x1b[1m--- /dev/null\x1b[m\n");
    output.push_str("\x1b[1m+++ ");
    output.push_str(&quote_diff_path(&new_path));
    output.push_str("\x1b[m");
    if path_needs_separator_tab(&new_path) {
        output.push('\t');
    }
    output.push('\n');
    for line in lines {
        if line.starts_with("@@") {
            output.push_str("\x1b[36m");
            output.push_str(line);
            output.push_str("\x1b[m\n");
        } else if let Some(inserted) = line.strip_prefix('+') {
            output.push_str("\x1b[32m+\x1b[m\x1b[32m");
            output.push_str(inserted);
            output.push_str("\x1b[m\n");
        } else if line == "\\ No newline at end of file" {
            output.push_str(line);
            output.push_str("\x1b[m\n");
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

fn git_blob_abbreviation(contents: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", contents.len()).as_bytes());
    hasher.update(contents);
    format!("{:x}", hasher.finalize())[..7].to_string()
}

fn path_needs_separator_tab(path: &str) -> bool {
    path.bytes().any(|byte| byte == b' ' || byte == b'\t')
}

fn quote_diff_path(path: &str) -> String {
    let needs_quotes = path
        .bytes()
        .any(|byte| !matches!(byte, 0x20..=0x21 | 0x23..=0x7e));
    if !needs_quotes {
        return path.to_string();
    }

    let mut quoted = String::from("\"");
    for byte in path.bytes() {
        match byte {
            b'\\' => quoted.push_str("\\\\"),
            b'\"' => quoted.push_str("\\\""),
            b'\n' => quoted.push_str("\\n"),
            b'\r' => quoted.push_str("\\r"),
            b'\t' => quoted.push_str("\\t"),
            0x20..=0x7e => quoted.push(char::from(byte)),
            _ => quoted.push_str(&format!("\\{byte:03o}")),
        }
    }
    quoted.push('\"');
    quoted
}

fn parse_untracked_files(output: &str) -> Result<Vec<PathBuf>, String> {
    let mut files = output
        .lines()
        .filter(|path| !path.is_empty())
        .map(decode_git_quoted_path)
        .collect::<Result<Vec<_>, _>>()?;
    files.sort_unstable();
    Ok(files)
}

fn decode_git_quoted_path(path: &str) -> Result<PathBuf, String> {
    let Some(quoted) = path.strip_prefix('"') else {
        return git_path_from_bytes(path.as_bytes().to_vec());
    };
    let quoted = quoted
        .strip_suffix('"')
        .ok_or_else(|| "unterminated quoted path from git ls-files".to_string())?;
    let bytes = quoted.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        let escaped = *bytes
            .get(index)
            .ok_or_else(|| "trailing escape in quoted git path".to_string())?;
        index += 1;
        match escaped {
            b'a' => decoded.push(0x07),
            b'b' => decoded.push(0x08),
            b't' => decoded.push(b'\t'),
            b'n' => decoded.push(b'\n'),
            b'v' => decoded.push(0x0b),
            b'f' => decoded.push(0x0c),
            b'r' => decoded.push(b'\r'),
            b'\\' | b'"' => decoded.push(escaped),
            b'0'..=b'7' => {
                let mut value = escaped - b'0';
                for _ in 0..2 {
                    let Some(next @ b'0'..=b'7') = bytes.get(index).copied() else {
                        break;
                    };
                    value = value.saturating_mul(8).saturating_add(next - b'0');
                    index += 1;
                }
                decoded.push(value);
            }
            _ => {
                return Err(format!(
                    "unsupported escape in quoted git path: \\{}",
                    char::from(escaped)
                ));
            }
        }
    }
    git_path_from_bytes(decoded)
}

#[cfg(unix)]
fn git_path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;

    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn git_path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf, String> {
    String::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|_| "git returned a path that is not valid UTF-8 on this platform".to_string())
}

fn append_omitted_untracked_diff_notice(diff: &mut String, omitted_files: &[PathBuf]) {
    if omitted_files.is_empty() {
        return;
    }

    if !diff.is_empty() {
        if !diff.ends_with('\n') {
            diff.push('\n');
        }
        diff.push('\n');
    }
    diff.push_str(&format!(
        "# Untracked file diffs omitted after first {MAX_UNTRACKED_FILE_DIFFS} files ({} omitted):\n",
        omitted_files.len()
    ));
    for file in omitted_files.iter().take(MAX_OMITTED_UNTRACKED_PATHS) {
        diff.push_str("# - ");
        diff.push_str(&escaped_path_for_notice(file));
        diff.push('\n');
    }
    let additional = omitted_files
        .len()
        .saturating_sub(MAX_OMITTED_UNTRACKED_PATHS);
    if additional > 0 {
        diff.push_str(&format!(
            "# ... {additional} additional omitted paths not listed\n"
        ));
    }
}

fn escaped_path_for_notice(path: &Path) -> String {
    let rendered = path.to_string_lossy();
    let mut chars = rendered.chars();
    let mut escaped = chars
        .by_ref()
        .take(MAX_OMITTED_UNTRACKED_PATH_CHARS)
        .flat_map(char::escape_default)
        .collect::<String>();
    if chars.next().is_some() {
        escaped.push('…');
    }
    escaped
}

/// Helper that executes `git` with the given `args` and returns `stdout` as a
/// UTF-8 string. Any non-zero exit status is considered an *error*.
async fn run_git_capture_stdout(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    fsmonitor: FsmonitorOverride,
    args: &[&str],
) -> Result<String, String> {
    let output = run_git_command(runner, cwd, fsmonitor, &[], args).await?;
    if output.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "git {:?} failed with status {}",
            args, output.exit_code
        ))
    }
}

/// Like [`run_git_capture_stdout`] but treats exit status 1 as success and
/// returns stdout. Git returns 1 for diffs when differences are present.
async fn run_git_capture_diff(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    fsmonitor: FsmonitorOverride,
    config_overrides: &[(String, String)],
    args: &[&str],
) -> Result<String, String> {
    let output = run_git_command(runner, cwd, fsmonitor, config_overrides, args).await?;
    capture_diff_output(output, args)
}

/// Executes a Git diff with a bounded response for executor-backed untracked-file fallbacks.
async fn run_git_capture_diff_bounded(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    fsmonitor: FsmonitorOverride,
    config_overrides: &[(String, String)],
    args: &[&str],
    complete_output_budget: usize,
) -> Result<BoundedDiffCapture, String> {
    let response_cap = complete_output_budget.saturating_add(1);
    let output = run_git_command_with_output_cap(
        runner,
        cwd,
        fsmonitor,
        config_overrides,
        args,
        Some(response_cap),
    )
    .await?;
    let output = capture_diff_output(output, args)?;
    let captured_bytes = output.len().min(response_cap);
    Ok(BoundedDiffCapture {
        output: (output.len() <= complete_output_budget).then_some(output),
        captured_bytes,
    })
}

struct BoundedDiffCapture {
    output: Option<String>,
    captured_bytes: usize,
}

fn capture_diff_output(output: WorkspaceCommandOutput, args: &[&str]) -> Result<String, String> {
    if output.success() || output.exit_code == 1 {
        Ok(output.stdout)
    } else {
        Err(format!(
            "git {:?} failed with status {}",
            args, output.exit_code
        ))
    }
}

/// Return Git configuration overrides that prevent configured filter drivers
/// from executing while generating diffs.
async fn diff_filter_config_overrides(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    fsmonitor: FsmonitorOverride,
) -> Result<Vec<(String, String)>, String> {
    let args = [
        "config",
        "--null",
        "--name-only",
        "--get-regexp",
        EXECUTABLE_FILTER_CONFIG_PATTERN,
    ];
    let output = run_git_command(runner, cwd, fsmonitor, &[], &args).await?;
    if output.exit_code != 0 && output.exit_code != 1 {
        return Err(format!(
            "git {:?} failed with status {}",
            args, output.exit_code
        ));
    }

    let mut drivers = output
        .stdout
        .split('\0')
        .filter_map(|key| {
            key.strip_suffix(".clean")
                .or_else(|| key.strip_suffix(".process"))
        })
        .map(str::to_string)
        .collect::<Vec<_>>();
    drivers.sort();
    drivers.dedup();

    Ok(drivers
        .into_iter()
        .flat_map(|driver| {
            [
                (format!("{driver}.clean"), String::new()),
                (format!("{driver}.process"), String::new()),
                (format!("{driver}.required"), "false".to_string()),
            ]
        })
        .collect())
}

/// Determine if the current directory is inside a Git repository.
async fn inside_git_repo(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Result<bool, String> {
    // `rev-parse` does not inspect the worktree, and probing before this check
    // would also run extra Git commands outside repositories.
    let output = run_git_command(
        runner,
        cwd,
        FsmonitorOverride::Disabled,
        &[],
        &["rev-parse", "--is-inside-work-tree"],
    )
    .await?;
    Ok(output.success())
}

async fn run_git_command(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    fsmonitor: FsmonitorOverride,
    config_overrides: &[(String, String)],
    args: &[&str],
) -> Result<WorkspaceCommandOutput, String> {
    run_git_command_with_output_cap(
        runner,
        cwd,
        fsmonitor,
        config_overrides,
        args,
        /*output_bytes_cap*/ None,
    )
    .await
}

async fn run_git_command_with_output_cap(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    fsmonitor: FsmonitorOverride,
    config_overrides: &[(String, String)],
    args: &[&str],
    output_bytes_cap: Option<usize>,
) -> Result<WorkspaceCommandOutput, String> {
    let argv = [
        "git",
        "-c",
        fsmonitor.git_config_arg(),
        "-c",
        DISABLE_HOOKS_CONFIG,
    ]
    .into_iter()
    .chain(args.iter().copied());
    let mut command = WorkspaceCommand::new(argv)
        .cwd(cwd.to_path_buf())
        .timeout(DIFF_COMMAND_TIMEOUT);
    command = match output_bytes_cap {
        Some(output_bytes_cap) => command.output_bytes_cap(output_bytes_cap),
        None => command.disable_output_cap(),
    };
    if !config_overrides.is_empty() {
        command = command.env("GIT_CONFIG_COUNT", config_overrides.len().to_string());
        for (index, (key, value)) in config_overrides.iter().enumerate() {
            command = command
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
    }
    runner.run(command).await.map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_command::WorkspaceCommandError;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    #[cfg(unix)]
    use std::fs;
    use std::future::Future;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::process::Command as ProcessCommand;
    use std::sync::Mutex;

    #[tokio::test]
    async fn get_git_diff_returns_not_git_for_non_git_cwd() {
        let cwd = PathBuf::from("/workspace");
        let runner = FakeRunner::new(vec![response(
            git_command(
                FsmonitorOverride::Disabled,
                &["rev-parse", "--is-inside-work-tree"],
            ),
            /*exit_code*/ 128,
            "",
        )]);

        let result = get_git_diff(&runner, &cwd).await;

        assert_eq!(result, Ok((false, String::new())));
        assert_command_metadata(&runner.commands(), &cwd);
    }

    #[tokio::test]
    async fn unreadable_untracked_file_uses_executor_git_fallback_without_helpers() {
        let cwd = PathBuf::from("/workspace");
        let runner = FakeRunner::new(vec![
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &["rev-parse", "--is-inside-work-tree"],
                ),
                /*exit_code*/ 0,
                "true\n",
            ),
            response(
                git_probe_command(&["config", "--null", "--get", "core.fsmonitor"]),
                /*exit_code*/ 0,
                "/tmp/fsmonitor-helper\0",
            ),
            response(
                git_probe_command(&[
                    "config",
                    "--null",
                    "--type=bool",
                    "--fixed-value",
                    "--get",
                    "core.fsmonitor",
                    "/tmp/fsmonitor-helper",
                ]),
                /*exit_code*/ 128,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "config",
                        "--null",
                        "--name-only",
                        "--get-regexp",
                        EXECUTABLE_FILTER_CONFIG_PATTERN,
                    ],
                ),
                /*exit_code*/ 0,
                "filter.evil.clean\0filter.evil.process\0",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "diff",
                        "--no-textconv",
                        "--no-ext-diff",
                        "--submodule=short",
                        "--ignore-submodules=dirty",
                        "--color",
                    ],
                ),
                /*exit_code*/ 1,
                "tracked\n",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "-c",
                        "core.quotePath=true",
                        "ls-files",
                        "--others",
                        "--exclude-standard",
                    ],
                ),
                /*exit_code*/ 0,
                "new.txt\n",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "diff",
                        "--no-textconv",
                        "--no-ext-diff",
                        "--submodule=short",
                        "--ignore-submodules=dirty",
                        "--color",
                        "--no-index",
                        "--",
                        null_device(),
                        "new.txt",
                    ],
                ),
                /*exit_code*/ 1,
                "untracked\n",
            ),
        ]);

        let result = get_git_diff(&runner, &cwd).await;

        assert_eq!(result, Ok((true, "tracked\nuntracked\n".to_string())));
        let commands = runner.commands();
        assert_command_metadata(&commands, &cwd);
        assert_eq!(commands[4].env, filter_override_env("filter.evil"));
        assert_eq!(commands[6].env, filter_override_env("filter.evil"));
    }

    #[tokio::test]
    async fn bounded_remote_diff_discards_output_that_reaches_the_sentinel() {
        let cwd = PathBuf::from("/workspace");
        let args = ["diff", "--no-index"];
        let complete_output_budget = 8;
        let runner = FakeRunner::new(vec![response(
            git_command(FsmonitorOverride::Disabled, &args),
            /*exit_code*/ 1,
            "123456789",
        )]);

        let capture = run_git_capture_diff_bounded(
            &runner,
            &cwd,
            FsmonitorOverride::Disabled,
            &[],
            &args,
            complete_output_budget,
        )
        .await
        .expect("bounded diff response");

        assert!(capture.output.is_none());
        assert_eq!(capture.captured_bytes, complete_output_budget + 1);
        let commands = runner.commands();
        assert_eq!(commands[0].output_bytes_cap, complete_output_budget + 1);
    }

    #[tokio::test]
    async fn remote_untracked_fallback_enforces_the_total_capture_budget() {
        let cwd = PathBuf::from("/workspace");
        let files = (0..5)
            .map(|index| format!("remote-{index}.txt"))
            .collect::<Vec<_>>();
        let untracked_output = format!("{}\n", files.join("\n"));
        let mut responses = vec![
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &["rev-parse", "--is-inside-work-tree"],
                ),
                /*exit_code*/ 0,
                "true\n",
            ),
            response(
                git_probe_command(&["config", "--null", "--get", "core.fsmonitor"]),
                /*exit_code*/ 1,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "config",
                        "--null",
                        "--name-only",
                        "--get-regexp",
                        EXECUTABLE_FILTER_CONFIG_PATTERN,
                    ],
                ),
                /*exit_code*/ 1,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "diff",
                        "--no-textconv",
                        "--no-ext-diff",
                        "--submodule=short",
                        "--ignore-submodules=dirty",
                        "--color",
                    ],
                ),
                /*exit_code*/ 0,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "-c",
                        "core.quotePath=true",
                        "ls-files",
                        "--others",
                        "--exclude-standard",
                    ],
                ),
                /*exit_code*/ 0,
                &untracked_output,
            ),
        ];
        let mut remaining_budget = MAX_UNTRACKED_TOTAL_BYTES as usize;
        let mut expected_caps = Vec::new();
        for file in files.iter().take(4) {
            let response_cap = (MAX_UNTRACKED_FILE_BYTES as usize + 1).min(remaining_budget);
            expected_caps.push(response_cap);
            let capped_output = "z".repeat(response_cap);
            responses.push(response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "diff",
                        "--no-textconv",
                        "--no-ext-diff",
                        "--submodule=short",
                        "--ignore-submodules=dirty",
                        "--color",
                        "--no-index",
                        "--",
                        null_device(),
                        file,
                    ],
                ),
                /*exit_code*/ 1,
                &capped_output,
            ));
            remaining_budget -= response_cap;
        }
        assert_eq!(remaining_budget, 0);
        let runner = FakeRunner::new(responses);

        let (_, diff) = get_git_diff(&runner, &cwd)
            .await
            .expect("bounded remote untracked diff");

        assert_eq!(
            diff.matches(
                "# Remote untracked file diff omitted because its complete output exceeds the bounded response budget:"
            )
            .count(),
            4
        );
        assert!(
            diff.contains(
                "# Remaining untracked file diffs omitted after bounded response budget\n"
            )
        );
        assert!(!diff.contains("zzzzzzzz"));
        let commands = runner.commands();
        let response_caps = commands
            .iter()
            .filter(|command| command.argv.iter().any(|arg| arg == "--no-index"))
            .map(|command| command.output_bytes_cap)
            .collect::<Vec<_>>();
        assert_eq!(response_caps, expected_caps);
        assert_eq!(commands.len(), 5 + expected_caps.len());
    }

    #[tokio::test]
    async fn get_git_diff_preserves_builtin_fsmonitor_for_diff_workflow() {
        let cwd = PathBuf::from("/workspace");
        let runner = FakeRunner::new(vec![
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &["rev-parse", "--is-inside-work-tree"],
                ),
                /*exit_code*/ 0,
                "true\n",
            ),
            response(
                git_probe_command(&["config", "--null", "--get", "core.fsmonitor"]),
                /*exit_code*/ 0,
                "true\0",
            ),
            response(
                git_probe_command(&["version", "--build-options"]),
                /*exit_code*/ 0,
                "feature: fsmonitor--daemon\n",
            ),
            response(
                git_command(
                    FsmonitorOverride::BuiltIn,
                    &[
                        "config",
                        "--null",
                        "--name-only",
                        "--get-regexp",
                        EXECUTABLE_FILTER_CONFIG_PATTERN,
                    ],
                ),
                /*exit_code*/ 1,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::BuiltIn,
                    &[
                        "diff",
                        "--no-textconv",
                        "--no-ext-diff",
                        "--submodule=short",
                        "--ignore-submodules=dirty",
                        "--color",
                    ],
                ),
                /*exit_code*/ 1,
                "tracked\n",
            ),
            response(
                git_command(
                    FsmonitorOverride::BuiltIn,
                    &[
                        "-c",
                        "core.quotePath=true",
                        "ls-files",
                        "--others",
                        "--exclude-standard",
                    ],
                ),
                /*exit_code*/ 0,
                "new.txt\n",
            ),
            response(
                git_command(
                    FsmonitorOverride::BuiltIn,
                    &[
                        "diff",
                        "--no-textconv",
                        "--no-ext-diff",
                        "--submodule=short",
                        "--ignore-submodules=dirty",
                        "--color",
                        "--no-index",
                        "--",
                        null_device(),
                        "new.txt",
                    ],
                ),
                /*exit_code*/ 1,
                "untracked\n",
            ),
        ]);

        let result = get_git_diff(&runner, &cwd).await;

        assert_eq!(result, Ok((true, "tracked\nuntracked\n".to_string())));
        assert_command_metadata(&runner.commands(), &cwd);
    }

    #[tokio::test]
    async fn get_git_diff_accepts_diff_exit_code_one() {
        let cwd = PathBuf::from("/workspace");
        let runner = FakeRunner::new(vec![
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &["rev-parse", "--is-inside-work-tree"],
                ),
                /*exit_code*/ 0,
                "true\n",
            ),
            response(
                git_probe_command(&["config", "--null", "--get", "core.fsmonitor"]),
                /*exit_code*/ 1,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "config",
                        "--null",
                        "--name-only",
                        "--get-regexp",
                        EXECUTABLE_FILTER_CONFIG_PATTERN,
                    ],
                ),
                /*exit_code*/ 1,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "diff",
                        "--no-textconv",
                        "--no-ext-diff",
                        "--submodule=short",
                        "--ignore-submodules=dirty",
                        "--color",
                    ],
                ),
                /*exit_code*/ 1,
                "tracked\n",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "-c",
                        "core.quotePath=true",
                        "ls-files",
                        "--others",
                        "--exclude-standard",
                    ],
                ),
                /*exit_code*/ 0,
                "",
            ),
        ]);

        let result = get_git_diff(&runner, &cwd).await;

        assert_eq!(result, Ok((true, "tracked\n".to_string())));
        assert_command_metadata(&runner.commands(), &cwd);
    }

    #[tokio::test]
    async fn get_git_diff_caps_untracked_file_diffs_and_lists_omitted_paths() {
        let cwd = PathBuf::from("/workspace");
        let untracked_files = (0..MAX_UNTRACKED_FILE_DIFFS + 2)
            .map(|index| format!("new-{index:02}.txt"))
            .collect::<Vec<_>>();
        let untracked_output = format!("{}\n", untracked_files.join("\n"));
        let mut responses = vec![
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &["rev-parse", "--is-inside-work-tree"],
                ),
                /*exit_code*/ 0,
                "true\n",
            ),
            response(
                git_probe_command(&["config", "--null", "--get", "core.fsmonitor"]),
                /*exit_code*/ 1,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "config",
                        "--null",
                        "--name-only",
                        "--get-regexp",
                        EXECUTABLE_FILTER_CONFIG_PATTERN,
                    ],
                ),
                /*exit_code*/ 1,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "diff",
                        "--no-textconv",
                        "--no-ext-diff",
                        "--submodule=short",
                        "--ignore-submodules=dirty",
                        "--color",
                    ],
                ),
                /*exit_code*/ 1,
                "tracked\n",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "-c",
                        "core.quotePath=true",
                        "ls-files",
                        "--others",
                        "--exclude-standard",
                    ],
                ),
                /*exit_code*/ 0,
                &untracked_output,
            ),
        ];
        let mut expected_diff = String::from("tracked\n");
        for (index, file) in untracked_files
            .iter()
            .take(MAX_UNTRACKED_FILE_DIFFS)
            .enumerate()
        {
            let output = format!("untracked-{index:02}\n");
            expected_diff.push_str(&output);
            responses.push(response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "diff",
                        "--no-textconv",
                        "--no-ext-diff",
                        "--submodule=short",
                        "--ignore-submodules=dirty",
                        "--color",
                        "--no-index",
                        "--",
                        null_device(),
                        file,
                    ],
                ),
                /*exit_code*/ 1,
                &output,
            ));
        }
        expected_diff.push_str(
            "\n# Untracked file diffs omitted after first 50 files (2 omitted):\n\
# - new-50.txt\n\
# - new-51.txt\n",
        );
        let runner = FakeRunner::new(responses);

        let result = get_git_diff(&runner, &cwd).await;

        assert_eq!(result, Ok((true, expected_diff)));
        let commands = runner.commands();
        assert_command_metadata(&commands, &cwd);
        assert_eq!(commands.len(), 5 + MAX_UNTRACKED_FILE_DIFFS);
    }

    #[tokio::test]
    async fn get_git_diff_rejects_unexpected_git_diff_status() {
        let cwd = PathBuf::from("/workspace");
        let runner = FakeRunner::new(vec![
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &["rev-parse", "--is-inside-work-tree"],
                ),
                /*exit_code*/ 0,
                "true\n",
            ),
            response(
                git_probe_command(&["config", "--null", "--get", "core.fsmonitor"]),
                /*exit_code*/ 1,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "config",
                        "--null",
                        "--name-only",
                        "--get-regexp",
                        EXECUTABLE_FILTER_CONFIG_PATTERN,
                    ],
                ),
                /*exit_code*/ 1,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "diff",
                        "--no-textconv",
                        "--no-ext-diff",
                        "--submodule=short",
                        "--ignore-submodules=dirty",
                        "--color",
                    ],
                ),
                /*exit_code*/ 2,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "-c",
                        "core.quotePath=true",
                        "ls-files",
                        "--others",
                        "--exclude-standard",
                    ],
                ),
                /*exit_code*/ 0,
                "",
            ),
        ]);

        let error = get_git_diff(&runner, &cwd)
            .await
            .expect_err("unexpected git diff status should fail");

        assert_eq!(
            error,
            "git [\"diff\", \"--no-textconv\", \"--no-ext-diff\", \"--submodule=short\", \"--ignore-submodules=dirty\", \"--color\"] failed with status 2"
        );
        assert_command_metadata(&runner.commands(), &cwd);
    }

    #[test]
    fn git_quoted_untracked_paths_preserve_whitespace_and_newlines() {
        assert_eq!(
            parse_untracked_files("\" leading\\nname.txt \"\n\"trailing-space.txt \"\n"),
            Ok(vec![
                PathBuf::from(" leading\nname.txt "),
                PathBuf::from("trailing-space.txt "),
            ])
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_quoted_untracked_paths_preserve_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let decoded = decode_git_quoted_path("\"dir/non-utf8-\\377.txt\"").unwrap();
        assert_eq!(decoded.as_os_str().as_bytes(), b"dir/non-utf8-\xff.txt");
    }

    #[test]
    fn omitted_untracked_notice_is_bounded_and_escapes_control_characters() {
        let paths = (0..MAX_OMITTED_UNTRACKED_PATHS + 2)
            .map(|index| format!("path-{index:02}\n{}.txt", "x".repeat(250)))
            .collect::<Vec<_>>();
        let path_refs = paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        let mut notice = String::new();

        append_omitted_untracked_diff_notice(&mut notice, &path_refs);

        assert_eq!(notice.matches("# - ").count(), MAX_OMITTED_UNTRACKED_PATHS);
        assert_eq!(notice.lines().count(), MAX_OMITTED_UNTRACKED_PATHS + 2);
        assert!(notice.contains("\\n"));
        assert!(notice.contains('…'));
        assert!(notice.contains("# ... 2 additional omitted paths not listed\n"));
        assert!(notice.lines().all(|line| line.len() < 2_100));
    }

    #[test]
    fn local_untracked_renderer_matches_observable_git_new_file_output() {
        assert_eq!(
            render_untracked_new_file("text.txt", "100644", b"hello\nworld\n"),
            concat!(
                "\x1b[1mdiff --git a/text.txt b/text.txt\x1b[m\n",
                "\x1b[1mnew file mode 100644\x1b[m\n",
                "\x1b[1mindex 0000000..94954ab\x1b[m\n",
                "\x1b[1m--- /dev/null\x1b[m\n",
                "\x1b[1m+++ b/text.txt\x1b[m\n",
                "\x1b[36m@@ -0,0 +1,2 @@\x1b[m\n",
                "\x1b[32m+\x1b[m\x1b[32mhello\x1b[m\n",
                "\x1b[32m+\x1b[m\x1b[32mworld\x1b[m\n",
            )
        );
        assert!(
            render_untracked_new_file("no-newline", "120000", b"target").ends_with(
                "\x1b[32m+\x1b[m\x1b[32mtarget\x1b[m\n\\ No newline at end of file\x1b[m\n"
            )
        );
        assert!(
            render_untracked_new_file("line\nbreak.txt", "100644", b"x\n").starts_with(
                "\x1b[1mdiff --git \"a/line\\nbreak.txt\" \"b/line\\nbreak.txt\"\x1b[m\n"
            )
        );
    }

    #[test]
    fn local_untracked_renderer_matches_git_for_text_binary_empty_and_newline_cases() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let cwd = tempdir.path();
        let cases = [
            ("text.txt", b"hello\nworld\n".as_slice()),
            ("binary.dat", b"a\0b".as_slice()),
            ("empty.txt", b"".as_slice()),
            ("no-newline.txt", b"last line".as_slice()),
            ("path with spaces.txt", b"quoted\n".as_slice()),
        ];
        for (path, contents) in cases {
            fs::write(cwd.join(path), contents).expect("write golden input");
            assert_eq!(
                render_local_untracked_file(cwd, Path::new(path), MAX_UNTRACKED_TOTAL_BYTES)
                    .expect("render untracked file")
                    .expect("file within budget")
                    .0,
                git_untracked_new_file_diff(cwd, path),
                "renderer differs from git for {path:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_untracked_renderer_matches_git_for_executable_symlink_and_quoted_path() {
        use std::os::unix::fs::symlink;

        let tempdir = tempfile::tempdir().expect("create temp directory");
        let cwd = tempdir.path();

        let executable = "executable.sh";
        fs::write(cwd.join(executable), "#!/bin/sh\nexit 0\n").expect("write executable");
        let mut permissions = fs::metadata(cwd.join(executable))
            .expect("read executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(cwd.join(executable), permissions).expect("mark executable");

        let link = "linked-file";
        symlink("target with spaces", cwd.join(link)).expect("create symlink");

        let quoted = "line\nbreak.txt";
        fs::write(cwd.join(quoted), "quoted\n").expect("write unusual path");

        for path in [executable, link, quoted] {
            assert_eq!(
                render_local_untracked_file(cwd, Path::new(path), MAX_UNTRACKED_TOTAL_BYTES)
                    .expect("render untracked file")
                    .expect("file within budget")
                    .0,
                git_untracked_new_file_diff(cwd, path),
                "renderer differs from git for {path:?}"
            );
        }
    }

    #[tokio::test]
    async fn local_untracked_files_do_not_launch_per_file_git_diffs() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let cwd = tempdir.path().to_path_buf();
        let files = (0..32)
            .map(|index| format!("new-{index:02}.txt"))
            .collect::<Vec<_>>();
        for file in &files {
            fs::write(cwd.join(file), format!("{file}\n")).expect("write local untracked file");
        }
        let untracked_output = files
            .iter()
            .map(|file| format!("{file}\n"))
            .collect::<String>();
        let runner = FakeRunner::new(vec![
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &["rev-parse", "--is-inside-work-tree"],
                ),
                0,
                "true\n",
            ),
            response(
                git_probe_command(&["config", "--null", "--get", "core.fsmonitor"]),
                1,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "config",
                        "--null",
                        "--name-only",
                        "--get-regexp",
                        EXECUTABLE_FILTER_CONFIG_PATTERN,
                    ],
                ),
                1,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "diff",
                        "--no-textconv",
                        "--no-ext-diff",
                        "--submodule=short",
                        "--ignore-submodules=dirty",
                        "--color",
                    ],
                ),
                0,
                "",
            ),
            response(
                git_command(
                    FsmonitorOverride::Disabled,
                    &[
                        "-c",
                        "core.quotePath=true",
                        "ls-files",
                        "--others",
                        "--exclude-standard",
                    ],
                ),
                0,
                &untracked_output,
            ),
        ]);

        let result = get_git_diff(&runner, &cwd)
            .await
            .expect("render local diff");

        assert!(result.0);
        assert_eq!(runner.commands().len(), 5);
        assert_eq!(result.1.matches("diff --git").count(), 32);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn get_git_diff_does_not_execute_configured_filters_fsmonitor_or_hooks() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let repo = tempdir.path().join("repo");
        fs::create_dir(&repo).expect("create test repository directory");
        run_git_setup(&repo, &["init", "-q"]);
        run_git_setup(&repo, &["config", "user.name", "test"]);
        run_git_setup(&repo, &["config", "user.email", "test@example.com"]);
        fs::write(repo.join(".gitattributes"), "*.txt filter=x=y\n").expect("write attributes");
        fs::write(repo.join("tracked.txt"), "before\n").expect("write tracked file");
        fs::write(repo.join("unchanged.txt"), "unchanged\n").expect("write unchanged file");
        run_git_setup(
            &repo,
            &["add", ".gitattributes", "tracked.txt", "unchanged.txt"],
        );
        run_git_setup(&repo, &["commit", "-qm", "initial"]);

        let filter_helper = tempdir.path().join("filter-helper.sh");
        let fsmonitor_helper = tempdir.path().join("fsmonitor-helper.sh");
        let hooks_dir = tempdir.path().join("hooks");
        let hook_helper = hooks_dir.join("post-index-change");
        fs::create_dir(&hooks_dir).expect("create hooks directory");
        write_marker_helper(&filter_helper);
        write_marker_helper(&fsmonitor_helper);
        write_marker_helper(&hook_helper);
        run_git_setup(
            &repo,
            &[
                "config",
                "filter.x=y.clean",
                filter_helper.to_str().expect("filter helper path"),
            ],
        );
        run_git_setup(
            &repo,
            &[
                "config",
                "filter.x=y.process",
                filter_helper.to_str().expect("filter helper path"),
            ],
        );
        run_git_setup(&repo, &["config", "filter.x=y.required", "true"]);
        run_git_setup(
            &repo,
            &[
                "config",
                "core.fsmonitor",
                fsmonitor_helper.to_str().expect("fsmonitor helper path"),
            ],
        );
        run_git_setup(
            &repo,
            &[
                "config",
                "core.hooksPath",
                hooks_dir.to_str().expect("hooks directory path"),
            ],
        );
        std::thread::sleep(Duration::from_secs(/*secs*/ 1));
        fs::write(repo.join("unchanged.txt"), "unchanged\n").expect("refresh unchanged file");
        fs::write(repo.join("tracked.txt"), "after\n").expect("modify tracked file");

        let result = get_git_diff(&LocalRunner, &repo)
            .await
            .expect("generate diff without invoking helpers");

        assert_eq!(
            (
                result.1.contains("before"),
                result.1.contains("after"),
                filter_helper.with_extension("sh.ran").exists(),
                fsmonitor_helper.with_extension("sh.ran").exists(),
                hook_helper.with_extension("sh.ran").exists(),
            ),
            (true, true, false, false, false),
            "diff:\n{}",
            result.1
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn get_git_diff_does_not_execute_helpers_while_checking_dirty_submodules() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let child = tempdir.path().join("child");
        let repo = tempdir.path().join("repo");
        fs::create_dir(&child).expect("create child repository directory");
        fs::create_dir(&repo).expect("create parent repository directory");
        run_git_setup(&child, &["init", "-q"]);
        run_git_setup(&child, &["config", "user.name", "test"]);
        run_git_setup(&child, &["config", "user.email", "test@example.com"]);
        fs::write(child.join(".gitattributes"), "*.txt filter=evil\n")
            .expect("write child attributes");
        fs::write(child.join("tracked.txt"), "before\n").expect("write child tracked file");
        run_git_setup(&child, &["add", ".gitattributes", "tracked.txt"]);
        run_git_setup(&child, &["commit", "-qm", "initial"]);

        run_git_setup(&repo, &["init", "-q"]);
        run_git_setup(&repo, &["config", "user.name", "test"]);
        run_git_setup(&repo, &["config", "user.email", "test@example.com"]);
        run_git_setup(
            &repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                child.to_str().expect("child repository path"),
                "child",
            ],
        );
        run_git_setup(&repo, &["commit", "-qm", "add submodule"]);

        let helper = tempdir.path().join("submodule-helper.sh");
        write_marker_helper(&helper);
        let checkout = repo.join("child");
        run_git_setup(
            &checkout,
            &[
                "config",
                "filter.evil.clean",
                helper.to_str().expect("submodule helper path"),
            ],
        );
        run_git_setup(&checkout, &["config", "filter.evil.required", "true"]);
        std::thread::sleep(Duration::from_secs(/*secs*/ 1));
        fs::write(checkout.join("tracked.txt"), "before\n").expect("refresh child tracked file");

        let result = get_git_diff(&LocalRunner, &repo)
            .await
            .expect("generate diff without inspecting submodule worktrees");

        assert_eq!(
            (result.1, helper.with_extension("sh.ran").exists()),
            (String::new(), false)
        );
    }

    fn git_command(fsmonitor: FsmonitorOverride, args: &[&str]) -> Vec<String> {
        [
            "git",
            "-c",
            fsmonitor.git_config_arg(),
            "-c",
            DISABLE_HOOKS_CONFIG,
        ]
        .into_iter()
        .chain(args.iter().copied())
        .map(str::to_string)
        .collect()
    }

    fn git_probe_command(args: &[&str]) -> Vec<String> {
        ["git"]
            .into_iter()
            .chain(args.iter().copied())
            .map(str::to_string)
            .collect()
    }

    fn filter_override_env(driver: &str) -> HashMap<String, Option<String>> {
        HashMap::from([
            ("GIT_CONFIG_COUNT".to_string(), Some("3".to_string())),
            (
                "GIT_CONFIG_KEY_0".to_string(),
                Some(format!("{driver}.clean")),
            ),
            ("GIT_CONFIG_VALUE_0".to_string(), Some(String::new())),
            (
                "GIT_CONFIG_KEY_1".to_string(),
                Some(format!("{driver}.process")),
            ),
            ("GIT_CONFIG_VALUE_1".to_string(), Some(String::new())),
            (
                "GIT_CONFIG_KEY_2".to_string(),
                Some(format!("{driver}.required")),
            ),
            ("GIT_CONFIG_VALUE_2".to_string(), Some("false".to_string())),
        ])
    }

    fn response(argv: Vec<String>, exit_code: i32, stdout: &str) -> FakeResponse {
        FakeResponse {
            argv,
            output: WorkspaceCommandOutput {
                exit_code,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        }
    }

    fn null_device() -> &'static str {
        if cfg!(windows) { "NUL" } else { "/dev/null" }
    }

    fn git_untracked_new_file_diff(cwd: &Path, path: &str) -> String {
        let output = ProcessCommand::new("git")
            .args(["-c", "core.fsmonitor=false", "-c", DISABLE_HOOKS_CONFIG])
            .args([
                "diff",
                "--no-index",
                "--no-textconv",
                "--no-ext-diff",
                "--submodule=short",
                "--ignore-submodules=dirty",
                "--color",
                "--",
                null_device(),
                path,
            ])
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .current_dir(cwd)
            .output()
            .expect("run git golden command");
        assert_eq!(
            output.status.code(),
            Some(1),
            "git golden command failed for {path:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git diff output should be UTF-8")
    }

    #[cfg(unix)]
    fn run_git_setup(cwd: &Path, args: &[&str]) {
        let output = ProcessCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("run git setup command");
        assert_eq!(
            output.status.code(),
            Some(0),
            "git setup command failed: {args:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    fn write_marker_helper(path: &Path) {
        fs::write(path, "#!/bin/sh\nprintf ran >> \"$0.ran\"\nexit 1\n")
            .expect("write helper script");
        let mut permissions = fs::metadata(path)
            .expect("read helper metadata")
            .permissions();
        permissions.set_mode(/*mode*/ 0o755);
        fs::set_permissions(path, permissions).expect("make helper executable");
    }

    fn assert_command_metadata(commands: &[WorkspaceCommand], cwd: &Path) {
        for command in commands {
            assert_eq!(command.cwd.as_deref(), Some(cwd));
            if matches!(
                command.argv.get(1).map(String::as_str),
                Some("config" | "version")
            ) {
                assert_eq!(command.env, HashMap::new());
                assert_eq!(command.timeout, Duration::from_secs(/*secs*/ 5));
                assert_eq!(command.output_bytes_cap, 64 * 1024);
                assert_eq!(command.disable_output_cap, false);
            } else if command.argv.iter().any(|arg| arg == "--no-index") {
                assert_eq!(command.timeout, DIFF_COMMAND_TIMEOUT);
                assert!(command.output_bytes_cap > 0);
                assert!(command.output_bytes_cap <= MAX_UNTRACKED_FILE_BYTES as usize + 1);
                assert_eq!(command.disable_output_cap, false);
            } else {
                assert_eq!(command.timeout, DIFF_COMMAND_TIMEOUT);
                assert_eq!(command.disable_output_cap, true);
            }
        }
    }

    struct FakeResponse {
        argv: Vec<String>,
        output: WorkspaceCommandOutput,
    }

    struct FakeRunner {
        responses: Mutex<VecDeque<FakeResponse>>,
        commands: Mutex<Vec<WorkspaceCommand>>,
    }

    impl FakeRunner {
        fn new(responses: Vec<FakeResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                commands: Mutex::new(Vec::new()),
            }
        }

        fn commands(&self) -> Vec<WorkspaceCommand> {
            assert_eq!(
                self.responses.lock().expect("responses lock").len(),
                0,
                "unused fake responses"
            );
            self.commands.lock().expect("commands lock").clone()
        }
    }

    impl WorkspaceCommandExecutor for FakeRunner {
        fn run(
            &self,
            command: WorkspaceCommand,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                let mut responses = self.responses.lock().expect("responses lock");
                let response = responses.pop_front().expect("missing fake response");
                assert_eq!(command.argv, response.argv);
                self.commands.lock().expect("commands lock").push(command);
                Ok(response.output)
            })
        }
    }

    #[cfg(unix)]
    struct LocalRunner;

    #[cfg(unix)]
    impl WorkspaceCommandExecutor for LocalRunner {
        fn run(
            &self,
            command: WorkspaceCommand,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                let mut process = ProcessCommand::new(&command.argv[0]);
                process
                    .args(&command.argv[1..])
                    .current_dir(command.cwd.expect("test command cwd"));
                for (key, value) in command.env {
                    match value {
                        Some(value) => {
                            process.env(key, value);
                        }
                        None => {
                            process.env_remove(key);
                        }
                    }
                }
                let output = process.output().expect("run test command");
                Ok(WorkspaceCommandOutput {
                    exit_code: output.status.code().expect("test command exit code"),
                    stdout: String::from_utf8(output.stdout).expect("utf8 stdout"),
                    stderr: String::from_utf8(output.stderr).expect("utf8 stderr"),
                })
            })
        }
    }
}
