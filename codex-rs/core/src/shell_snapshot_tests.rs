use super::*;
#[cfg(unix)]
use crate::tools::runtimes::RuntimePathPrepends;
#[cfg(unix)]
use crate::tools::runtimes::maybe_wrap_shell_lc_with_snapshot;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command as StdCommand;

use tempfile::tempdir;

#[cfg(unix)]
const FILTERED_ENV_CHILD: &str = "CODEX_PHASE77_FILTERED_ENV_CHILD";
#[cfg(unix)]
const FILTERED_ENV_SENTINEL: &str = "CODEX_PHASE77_FILTERED_ENV_SENTINEL";
#[cfg(unix)]
const FILTERED_ENV_SENTINEL_VALUE: &str = "phase77-must-not-be-restored";
#[cfg(unix)]
const FILTERED_ENV_CHILD_SUCCESS: &str = "phase77-filtered-environment-child-complete";
#[cfg(unix)]
const SHELL_WRAPPER_DELEGATE_ENV: &str = "CODEX_PHASE77_SHELL_WRAPPER_DELEGATE";
#[cfg(unix)]
const SHELL_WRAPPER_LOG_ENV: &str = "CODEX_PHASE77_SHELL_WRAPPER_LOG";

fn current_environment() -> HashMap<String, String> {
    std::env::vars().collect()
}

#[cfg(unix)]
async fn write_logging_shell_wrapper(path: &Path) -> Result<()> {
    fs::write(
        path,
        r#"#!/bin/sh
printf '%s\n' "$1" >> "$CODEX_PHASE77_SHELL_WRAPPER_LOG"
exec "$CODEX_PHASE77_SHELL_WRAPPER_DELEGATE" "$@"
"#,
    )
    .await?;
    let mut permissions = fs::metadata(path).await?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).await?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct ProcessCleanup {
    pids: Vec<i32>,
}

#[cfg(target_os = "linux")]
impl Drop for ProcessCleanup {
    fn drop(&mut self) {
        for pid in &self.pids {
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: i32) -> Result<bool> {
    if unsafe { libc::kill(pid, 0) } == 0 {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some((_, state_and_rest)) = stat.rsplit_once(") ")
            && matches!(state_and_rest.as_bytes().first(), Some(b'Z' | b'X'))
        {
            return Ok(false);
        }
        return Ok(true);
    }

    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(err.into()),
    }
}

#[cfg(target_os = "linux")]
async fn wait_for_process_exit(pid: i32, label: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if !process_is_alive(pid)? {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed-out snapshot {label} with pid {pid} is still alive");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(unix)]
struct BlockingStdinPipe {
    original: i32,
    write_end: i32,
}

#[cfg(unix)]
impl BlockingStdinPipe {
    fn install() -> Result<Self> {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
            return Err(std::io::Error::last_os_error()).context("create stdin pipe");
        }

        let original = unsafe { libc::dup(libc::STDIN_FILENO) };
        if original == -1 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(err).context("dup stdin");
        }

        if unsafe { libc::dup2(fds[0], libc::STDIN_FILENO) } == -1 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
                libc::close(original);
            }
            return Err(err).context("replace stdin");
        }

        unsafe {
            libc::close(fds[0]);
        }

        Ok(Self {
            original,
            write_end: fds[1],
        })
    }
}

#[cfg(unix)]
impl Drop for BlockingStdinPipe {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.original, libc::STDIN_FILENO);
            libc::close(self.original);
            libc::close(self.write_end);
        }
    }
}

fn assert_snapshot_section(snapshot: &str, section: &str) {
    assert!(
        snapshot.lines().any(|line| line == section),
        "snapshot should contain exact section header {section:?}; snapshot={snapshot:?}"
    );
}

#[cfg(not(target_os = "windows"))]
fn assert_posix_snapshot_sections(snapshot: &str) {
    for section in [
        "# Snapshot file",
        "# Functions",
        "# setopts",
        "# aliases",
        "# exports",
    ] {
        assert_snapshot_section(snapshot, section);
    }
    assert_snapshot_section(snapshot, POSIX_SNAPSHOT_FORMAT_HEADER);
    assert!(
        snapshot.contains("PATH"),
        "snapshot should capture a PATH export"
    );
}

async fn get_snapshot(
    shell: &Shell,
    environment_variables: &HashMap<String, String>,
) -> Result<String> {
    let dir = tempdir()?;
    let path = dir.path().join("snapshot.sh");
    write_shell_snapshot(shell, &path.abs(), &dir.path().abs(), environment_variables).await?;
    let content = fs::read_to_string(&path).await?;
    Ok(content)
}

#[test]
fn strip_snapshot_preamble_removes_leading_output() {
    let snapshot = "noise\n# Snapshot file\nexport PATH=/bin\n";
    let cleaned = strip_snapshot_preamble(snapshot).expect("snapshot marker exists");
    assert_eq!(cleaned, "# Snapshot file\nexport PATH=/bin\n");
}

#[test]
fn strip_snapshot_preamble_requires_marker() {
    let result = strip_snapshot_preamble("missing header");
    assert!(result.is_err());
}

#[test]
fn snapshot_file_name_parser_supports_legacy_and_suffixed_names() {
    let session_id = "019cf82b-6a62-7700-bbbd-46909794ef89";

    assert_eq!(
        snapshot_session_id_from_file_name(&format!("{session_id}.sh")),
        Some(session_id)
    );
    assert_eq!(
        snapshot_session_id_from_file_name(&format!("{session_id}.123.sh")),
        Some(session_id)
    );
    assert_eq!(
        snapshot_session_id_from_file_name(&format!("{session_id}.tmp-123")),
        Some(session_id)
    );
    assert_eq!(
        snapshot_session_id_from_file_name("not-a-snapshot.txt"),
        None
    );
}

#[cfg(unix)]
#[tokio::test]
async fn try_create_uses_configured_shell_for_capture_and_validation() -> Result<()> {
    let dir = tempdir()?;
    let delegate = crate::shell::get_shell(ShellType::Bash, /*path*/ None)
        .context("bash is required for configured shell path test")?;
    let wrapper_path = dir.path().join("configured-bash");
    let invocation_log = dir.path().join("configured-bash-invocations");
    let codex_home = dir.path().join("codex-home").abs();
    fs::create_dir_all(&codex_home).await?;
    write_logging_shell_wrapper(&wrapper_path).await?;

    let shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: wrapper_path,
    };
    let mut environment = current_environment();
    environment.insert(
        SHELL_WRAPPER_DELEGATE_ENV.to_string(),
        delegate.shell_path.to_string_lossy().into_owned(),
    );
    environment.insert(
        SHELL_WRAPPER_LOG_ENV.to_string(),
        invocation_log.to_string_lossy().into_owned(),
    );
    environment.insert("BASH_ENV".to_string(), "/dev/null".to_string());
    environment.insert(
        "HOME".to_string(),
        dir.path().to_string_lossy().into_owned(),
    );

    let snapshot = ShellSnapshot::try_create(
        &codex_home,
        ThreadId::new(),
        &dir.path().abs(),
        &shell,
        &environment,
        /*state_db*/ None,
    )
    .await
    .expect("configured shell should create and validate the snapshot");

    let invocations = fs::read_to_string(&invocation_log).await?;
    assert_eq!(
        invocations.lines().collect::<Vec<_>>(),
        vec!["-lc", "-c"],
        "the configured executable must run the login capture before non-login validation"
    );

    drop(snapshot);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn try_create_does_not_capture_or_restore_filtered_environment() -> Result<()> {
    if std::env::var_os(FILTERED_ENV_CHILD).as_deref() != Some(OsStr::new("1")) {
        let output = StdCommand::new(std::env::current_exe()?)
            .arg("--nocapture")
            .arg("try_create_does_not_capture_or_restore_filtered_environment")
            .env(FILTERED_ENV_CHILD, "1")
            .env(FILTERED_ENV_SENTINEL, FILTERED_ENV_SENTINEL_VALUE)
            .output()?;
        assert!(
            output.status.success(),
            "filtered-environment subprocess failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(FILTERED_ENV_CHILD_SUCCESS),
            "filtered-environment subprocess exited without running the child assertions"
        );
        return Ok(());
    }

    assert_eq!(
        std::env::var(FILTERED_ENV_SENTINEL)?,
        FILTERED_ENV_SENTINEL_VALUE,
        "the child process must begin with the sentinel in its inherited environment"
    );

    let dir = tempdir()?;
    let cwd = dir.path().abs();
    let shell = crate::shell::get_shell(ShellType::Sh, /*path*/ None)
        .context("sh is required for filtered environment test")?;
    let mut environment = current_environment();
    environment.remove(FILTERED_ENV_CHILD);
    environment.remove(FILTERED_ENV_SENTINEL);

    let snapshot = ShellSnapshot::try_create(
        &cwd,
        ThreadId::new(),
        &cwd,
        &shell,
        &environment,
        /*state_db*/ None,
    )
    .await
    .expect("filtered environment should create and validate the snapshot");
    let snapshot_contents = fs::read_to_string(&snapshot.path).await?;
    assert!(!snapshot_contents.contains(FILTERED_ENV_SENTINEL));
    assert!(!snapshot_contents.contains(FILTERED_ENV_SENTINEL_VALUE));

    let script_args = [
        OsStr::new("codex-phase77-filtered-environment"),
        snapshot.path.as_os_str(),
    ];
    let restored = run_script_with_timeout_with_args(
        &shell,
        r#"\command . "$1"
if [ "${CODEX_PHASE77_FILTERED_ENV_SENTINEL+x}" = x ]; then
  \command printf restored
else
  \command printf absent
fi"#,
        &script_args,
        Duration::from_secs(5),
        /*use_login_shell*/ false,
        &cwd,
        &environment,
    )
    .await?;
    assert_eq!(restored, "absent");
    println!("{FILTERED_ENV_CHILD_SUCCESS}");

    drop(snapshot);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn try_create_treats_generated_snapshot_path_as_literal_data() -> Result<()> {
    let dir = tempdir()?;
    let cwd = dir.path().abs();
    let marker_path = cwd.join("phase77-path-was-evaluated");
    let codex_home = cwd.join("codex home $(printf injected > phase77-path-was-evaluated)");
    fs::create_dir_all(&codex_home).await?;
    let shell = crate::shell::get_shell(ShellType::Sh, /*path*/ None)
        .context("sh is required for literal snapshot path test")?;

    let snapshot = ShellSnapshot::try_create(
        &codex_home,
        ThreadId::new(),
        &cwd,
        &shell,
        &current_environment(),
        /*state_db*/ None,
    )
    .await
    .expect("metacharacters in the generated snapshot path must remain literal");

    assert!(
        !marker_path.exists(),
        "generated snapshot path was evaluated as shell syntax"
    );
    drop(snapshot);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn validation_treats_snapshot_path_as_positional_data() -> Result<()> {
    let dir = tempdir()?;
    let cwd = dir.path().abs();
    let marker_path = cwd.join("validation-path-was-interpolated");
    let snapshot_dir = cwd.join("snapshot-$(printf injected > validation-path-was-interpolated)");
    fs::create_dir_all(&snapshot_dir).await?;
    let snapshot_path = snapshot_dir.join("snapshot.sh");
    fs::write(&snapshot_path, "# Snapshot file\n:\n").await?;
    let shell = crate::shell::get_shell(ShellType::Sh, /*path*/ None)
        .context("sh is required for shell snapshot validation test")?;

    let validation = validate_snapshot(&shell, &snapshot_path, &cwd, &HashMap::new()).await;

    assert!(
        !marker_path.exists(),
        "snapshot path was evaluated as shell source instead of passed as positional data"
    );
    validation
}

#[cfg(unix)]
#[test]
fn bash_snapshot_filters_invalid_exports() -> Result<()> {
    let output = StdCommand::new("/bin/bash")
        .arg("-c")
        .arg(bash_snapshot_script())
        .env("BASH_ENV", "/dev/null")
        .env("VALID_NAME", "ok")
        .env("alias_lines", "original")
        .env("__CODEX_SNAPSHOT_OVERRIDE_0", "must-not-be-captured")
        .env("__CODEX_SNAPSHOT_ALIAS_LINES", "must-not-be-captured")
        .env("PWD", "/tmp/stale")
        .env("NEXTEST_BIN_EXE_codex-write-config-schema", "/path/to/bin")
        .env("BAD-NAME", "broken")
        .output()?;

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("VALID_NAME"));
    assert!(stdout.contains("alias_lines=\"original\""));
    assert!(!stdout.contains("__CODEX_SNAPSHOT_OVERRIDE_0"));
    assert!(!stdout.contains("__CODEX_SNAPSHOT_ALIAS_LINES"));
    assert!(!stdout.contains("PWD=/tmp/stale"));
    assert!(!stdout.contains("NEXTEST_BIN_EXE_codex-write-config-schema"));
    assert!(!stdout.contains("BAD-NAME"));

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn bash_snapshot_rejects_shadowed_startup_dispatchers() -> Result<()> {
    let dir = tempdir()?;
    let shell = crate::shell::get_shell(ShellType::Bash, /*path*/ None)
        .context("bash is required for dispatcher-shadow capture test")?;

    for (name, startup) in [
        (
            "bash-env-command",
            "printf '# Snapshot file\\n# Codex shell snapshot format: 3\\n'\ncommand() { printf command-hijack; }\n",
        ),
        ("bash-env-builtin", "builtin() { printf builtin-hijack; }\n"),
    ] {
        let startup_path = dir.path().join(name);
        let output_path = dir.path().join(format!("{name}.snapshot"));
        fs::write(&startup_path, startup).await?;
        let mut environment = current_environment();
        environment.insert(
            "BASH_ENV".to_string(),
            startup_path.to_string_lossy().into_owned(),
        );

        let err = write_shell_snapshot(&shell, &output_path.abs(), &dir.path().abs(), &environment)
            .await
            .expect_err("shadowed dispatcher must not produce a usable snapshot");

        let message = err.to_string();
        assert!(
            message.contains("exited with status"),
            "dispatcher-shadow capture should fail closed; err={err:?}"
        );
        assert!(!output_path.exists());
    }

    Ok(())
}

#[cfg(unix)]
#[test]
fn bash_snapshot_restores_shell_state_end_to_end() -> Result<()> {
    let multiline_value = "line one\nline 'two'\nline three";
    let shell = crate::shell::get_shell(ShellType::Bash, /*path*/ None)
        .context("bash is required for shell snapshot restoration test")?;
    let capture_script = format!(
        r#"snapshot_function() {{ printf function; }}
snapshot_extglob() {{ case foo in +(foo)) printf extglob ;; esac; }}
shopt() {{ builtin printf shopt-function; }}
declare() {{ builtin printf declare-function; }}
eval() {{ builtin printf eval-function; }}
exec() {{ builtin printf exec-function; }}
export() {{ builtin printf export-function; }}
unset() {{ builtin printf unset-function; }}
alias snapshot_alias='printf alias'
alias echo='printf "ALIASED:%s\n"'
set -o noclobber
builtin shopt -s expand_aliases nullglob
{}"#,
        bash_snapshot_script()
    );
    let output = StdCommand::new(&shell.shell_path)
        .arg("-O")
        .arg("extglob")
        .arg("-c")
        .arg(capture_script)
        .env("BASH_ENV", "/dev/null")
        .env("SNAPSHOT_MULTILINE", multiline_value)
        .output()?;

    assert!(
        output.status.success(),
        "snapshot capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot = strip_snapshot_preamble(&String::from_utf8(output.stdout)?)?;
    assert_posix_snapshot_sections(&snapshot);
    assert_snapshot_section(&snapshot, "# shopts");
    let shopts_index = snapshot
        .lines()
        .position(|line| line == "# shopts")
        .expect("shopts section exists");
    let functions_index = snapshot
        .lines()
        .position(|line| line == "# Functions")
        .expect("functions section exists");
    assert!(
        shopts_index < functions_index,
        "Bash shopt state must be restored before function definitions are parsed"
    );

    let dir = tempdir()?;
    let snapshot_path = dir.path().join("snapshot.sh");
    let restore_bash_env = dir.path().join("restore-bash-env");
    std::fs::write(&snapshot_path, snapshot)?;
    std::fs::write(
        &restore_bash_env,
        "shopt -s expand_aliases\nalias builtin=':'\nalias command=':'\nalias unalias=':'\nalias shopt=':'\nalias .=':'\nbuiltin() { printf BASH_ENV-builtin-hijack; }\ncommand() { printf BASH_ENV-command-hijack; }\n",
    )?;
    let original_script = r#"snapshot_function
printf '\036'
snapshot_extglob
printf '\036'
shopt
printf '\036'
declare
printf '\036'
eval
printf '\036'
exec
printf '\036'
export
printf '\036'
unset
printf '\036'
snapshot_alias
printf '\036'
echo snapshot_echo
printf '\036'
if [[ -o noclobber ]]; then printf noclobber; else printf missing-noclobber; fi
printf '\036'
if builtin shopt -q nullglob; then printf nullglob; else printf missing-nullglob; fi
printf '\036'
if builtin shopt -q login_shell; then printf login-shell; else printf missing-login-shell; fi
printf '\036%s\036%s\036%s' "$SNAPSHOT_OVERRIDE" "$SNAPSHOT_MULTILINE" "$BASH_ENV""#;
    let command = shell.derive_exec_args(original_script, /*use_login_shell*/ true);
    let wrapped = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &shell,
        Some(&snapshot_path.abs()),
        &HashMap::from([("SNAPSHOT_OVERRIDE".to_string(), "worktree".to_string())]),
        &HashMap::from([("SNAPSHOT_OVERRIDE".to_string(), "worktree".to_string())]),
        &RuntimePathPrepends::default(),
    );
    let restored = StdCommand::new(&wrapped[0])
        .args(&wrapped[1..])
        .env("BASH_ENV", restore_bash_env)
        .env("SNAPSHOT_OVERRIDE", "worktree")
        .output()?;

    assert!(
        restored.status.success(),
        "restored command failed: {}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(
        String::from_utf8(restored.stdout)?,
        format!(
            "function\u{001e}extglob\u{001e}shopt-function\u{001e}declare-function\u{001e}eval-function\u{001e}exec-function\u{001e}export-function\u{001e}unset-function\u{001e}alias\u{001e}ALIASED:snapshot_echo\n\u{001e}noclobber\u{001e}nullglob\u{001e}login-shell\u{001e}worktree\u{001e}{multiline_value}\u{001e}/dev/null"
        )
    );

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn try_create_creates_and_deletes_snapshot_file() -> Result<()> {
    let dir = tempdir()?;
    let shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/bin/bash"),
    };

    let snapshot = ShellSnapshot::try_create(
        &dir.path().abs(),
        ThreadId::new(),
        &dir.path().abs(),
        &shell,
        &current_environment(),
        /*state_db*/ None,
    )
    .await
    .expect("snapshot should be created");
    let path = snapshot.path.clone();
    assert!(path.exists());

    drop(snapshot);

    assert!(!path.exists());

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn try_create_uses_distinct_generation_paths() -> Result<()> {
    let dir = tempdir()?;
    let session_id = ThreadId::new();
    let shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/bin/bash"),
    };

    let initial_snapshot = ShellSnapshot::try_create(
        &dir.path().abs(),
        session_id,
        &dir.path().abs(),
        &shell,
        &current_environment(),
        /*state_db*/ None,
    )
    .await
    .expect("initial snapshot should be created");
    let refreshed_snapshot = ShellSnapshot::try_create(
        &dir.path().abs(),
        session_id,
        &dir.path().abs(),
        &shell,
        &current_environment(),
        /*state_db*/ None,
    )
    .await
    .expect("refreshed snapshot should be created");
    let initial_path = initial_snapshot.path.clone();
    let refreshed_path = refreshed_snapshot.path.clone();
    assert_ne!(initial_path, refreshed_path);
    assert_eq!(initial_path.exists(), true);
    assert_eq!(refreshed_path.exists(), true);

    drop(initial_snapshot);

    assert_eq!(initial_path.exists(), false);
    assert_eq!(refreshed_path.exists(), true);

    drop(refreshed_snapshot);

    assert_eq!(refreshed_path.exists(), false);

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_shell_does_not_inherit_stdin() -> Result<()> {
    let _stdin_guard = BlockingStdinPipe::install()?;

    let dir = tempdir()?;
    let home = dir.path().abs();
    let read_status_path = home.join("stdin-read-status");
    let read_status_display = read_status_path.display();
    // Persist the startup `read` exit status so the test can assert whether
    // bash saw EOF on stdin after the snapshot process exits.
    let bashrc = format!("read -t 1 -r ignored\nprintf '%s' \"$?\" > \"{read_status_display}\"\n");
    fs::write(home.join(".bashrc"), bashrc).await?;

    let shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/bin/bash"),
    };

    let home_display = home.display();
    let script = format!(
        "HOME=\"{home_display}\"; export HOME; {}",
        bash_snapshot_script()
    );
    let output = run_script_with_timeout(
        &shell,
        &script,
        Duration::from_secs(2),
        /*use_login_shell*/ true,
        &home,
        &current_environment(),
    )
    .await
    .context("run snapshot command")?;
    let read_status = fs::read_to_string(&read_status_path)
        .await
        .context("read stdin probe status")?;

    assert_eq!(
        read_status, "1",
        "expected shell startup read to see EOF on stdin; status={read_status:?}"
    );

    assert!(
        output.contains("# Snapshot file"),
        "expected snapshot marker in output; output={output:?}"
    );

    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn timed_out_snapshot_shell_and_child_are_terminated() -> Result<()> {
    let dir = tempdir()?;
    let pid_path = dir.path().join("pid");
    let script = format!(
        "sleep 30 & child_pid=$!; printf '%s\\n%s\\n' \"$$\" \"$child_pid\" > \"{}\"; wait \"$child_pid\"",
        pid_path.display()
    );

    let shell = Shell {
        shell_type: ShellType::Sh,
        shell_path: PathBuf::from("/bin/sh"),
    };

    let err = run_script_with_timeout(
        &shell,
        &script,
        Duration::from_secs(1),
        /*use_login_shell*/ true,
        &dir.path().abs(),
        &current_environment(),
    )
    .await
    .expect_err("snapshot shell should time out");
    assert!(
        err.to_string().contains("timed out"),
        "expected timeout error, got {err:?}"
    );

    let pid_contents = fs::read_to_string(&pid_path)
        .await
        .expect("snapshot shell writes both pids before timing out");
    let mut cleanup = ProcessCleanup::default();
    for line in pid_contents.lines() {
        let pid = line.parse::<i32>()?;
        if pid <= 1 {
            bail!("snapshot test recorded unsafe pid {pid}");
        }
        cleanup.pids.push(pid);
    }
    assert_eq!(
        cleanup.pids.len(),
        2,
        "expected shell and child pids; contents={pid_contents:?}"
    );
    let shell_pid = cleanup.pids[0];
    let child_pid = cleanup.pids[1];
    assert_ne!(shell_pid, child_pid);

    wait_for_process_exit(shell_pid, "shell group leader").await?;
    wait_for_process_exit(child_pid, "child process").await?;
    cleanup.pids.clear();

    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_zsh_snapshot_includes_sections() -> Result<()> {
    let shell = crate::shell::get_shell(ShellType::Zsh, /*path*/ None)
        .context("zsh is required for snapshot test")?;
    let snapshot = get_snapshot(&shell, &current_environment()).await?;
    assert_posix_snapshot_sections(&snapshot);
    let setopts_index = snapshot
        .lines()
        .position(|line| line == "# setopts")
        .expect("setopts section exists");
    let functions_index = snapshot
        .lines()
        .position(|line| line == "# Functions")
        .expect("functions section exists");
    assert!(
        setopts_index < functions_index,
        "Zsh option state must be restored before function definitions are parsed"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_bash_snapshot_includes_sections() -> Result<()> {
    let shell = crate::shell::get_shell(ShellType::Bash, /*path*/ None)
        .context("bash is required for snapshot test")?;
    let snapshot = get_snapshot(&shell, &current_environment()).await?;
    assert_posix_snapshot_sections(&snapshot);
    assert_snapshot_section(&snapshot, "# shopts");
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_sh_snapshot_includes_sections() -> Result<()> {
    let shell = crate::shell::get_shell(ShellType::Sh, /*path*/ None)
        .context("sh is required for snapshot test")?;
    let snapshot = get_snapshot(&shell, &current_environment()).await?;
    assert_posix_snapshot_sections(&snapshot);
    Ok(())
}

#[cfg(target_os = "windows")]
#[ignore]
#[tokio::test]
async fn windows_powershell_snapshot_includes_sections() -> Result<()> {
    let shell = crate::shell::get_shell(ShellType::PowerShell, /*path*/ None)
        .context("PowerShell is required for snapshot test")?;
    let snapshot = get_snapshot(&shell, &current_environment()).await?;
    for section in ["# Snapshot file", "# Functions", "# aliases", "# exports"] {
        assert_snapshot_section(&snapshot, section);
    }
    Ok(())
}

async fn write_rollout_stub(codex_home: &Path, session_id: ThreadId) -> Result<PathBuf> {
    let dir = codex_home
        .join("sessions")
        .join("2025")
        .join("01")
        .join("01");
    fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("rollout-2025-01-01T00-00-00-{session_id}.jsonl"));
    fs::write(&path, "").await?;
    Ok(path)
}

#[tokio::test]
async fn cleanup_stale_snapshots_removes_orphans_and_keeps_live() -> Result<()> {
    let dir = tempdir()?;
    let codex_home = dir.path().abs();
    let snapshot_dir = codex_home.join(SNAPSHOT_DIR);
    fs::create_dir_all(&snapshot_dir).await?;

    let live_session = ThreadId::new();
    let orphan_session = ThreadId::new();
    let live_snapshot = snapshot_dir.join(format!("{live_session}.123.sh"));
    let orphan_snapshot = snapshot_dir.join(format!("{orphan_session}.456.sh"));
    let invalid_snapshot = snapshot_dir.join("not-a-snapshot.txt");

    write_rollout_stub(&codex_home, live_session).await?;
    fs::write(&live_snapshot, "live").await?;
    fs::write(&orphan_snapshot, "orphan").await?;
    fs::write(&invalid_snapshot, "invalid").await?;

    cleanup_stale_snapshots(&codex_home, ThreadId::new(), /*state_db*/ None).await?;

    assert_eq!(live_snapshot.exists(), true);
    assert_eq!(orphan_snapshot.exists(), false);
    assert_eq!(invalid_snapshot.exists(), false);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn cleanup_stale_snapshots_removes_stale_rollouts() -> Result<()> {
    let dir = tempdir()?;
    let codex_home = dir.path().abs();
    let snapshot_dir = codex_home.join(SNAPSHOT_DIR);
    fs::create_dir_all(&snapshot_dir).await?;

    let stale_session = ThreadId::new();
    let stale_snapshot = snapshot_dir.join(format!("{stale_session}.123.sh"));
    let rollout_path = write_rollout_stub(&codex_home, stale_session).await?;
    fs::write(&stale_snapshot, "stale").await?;

    set_file_mtime(&rollout_path, SNAPSHOT_RETENTION + Duration::from_secs(60))?;

    cleanup_stale_snapshots(&codex_home, ThreadId::new(), /*state_db*/ None).await?;

    assert_eq!(stale_snapshot.exists(), false);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn cleanup_stale_snapshots_skips_active_session() -> Result<()> {
    let dir = tempdir()?;
    let codex_home = dir.path().abs();
    let snapshot_dir = codex_home.join(SNAPSHOT_DIR);
    fs::create_dir_all(&snapshot_dir).await?;

    let active_session = ThreadId::new();
    let active_snapshot = snapshot_dir.join(format!("{active_session}.123.sh"));
    let rollout_path = write_rollout_stub(&codex_home, active_session).await?;
    fs::write(&active_snapshot, "active").await?;

    set_file_mtime(&rollout_path, SNAPSHOT_RETENTION + Duration::from_secs(60))?;

    cleanup_stale_snapshots(&codex_home, active_session, /*state_db*/ None).await?;

    assert_eq!(active_snapshot.exists(), true);
    Ok(())
}

#[cfg(unix)]
fn set_file_mtime(path: &Path, age: Duration) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs()
        .saturating_sub(age.as_secs());
    let tv_sec = now
        .try_into()
        .map_err(|_| anyhow!("Snapshot mtime is out of range for libc::timespec"))?;
    let ts = libc::timespec { tv_sec, tv_nsec: 0 };
    let times = [ts, ts];
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let result = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
