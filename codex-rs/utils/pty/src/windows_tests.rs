use super::collect_output_until_exit;
use super::combine_spawned_output;
use super::find_python;
use super::wait_for_output_contains;
use crate::TerminalSize;
use crate::spawn_pipe_process_no_stdin;
use crate::spawn_pty_process;
use std::collections::HashMap;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::time::Duration;

const READY_MARKER: &str = "__CODEX_CHILD_READY__";
const VALUE_MARKER: &str = "__CODEX_CHILD_VALUE__";
const REQUIRE_PROCESS_TESTS_ENV: &str = "CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS";

struct WindowsShell {
    name: &'static str,
    program: String,
    args: Vec<String>,
    child_command: String,
}

fn find_powershell() -> Option<String> {
    ["pwsh.exe", "powershell.exe"]
        .into_iter()
        .find_map(|candidate| {
            std::process::Command::new(candidate)
                .args(["-NoLogo", "-NoProfile", "-Command", "exit 0"])
                .status()
                .ok()
                .filter(std::process::ExitStatus::success)
                .map(|_| candidate.to_string())
        })
}

fn process_test_prerequisite<T>(
    value: Option<T>,
    prerequisite: &str,
    test: &str,
    required: bool,
) -> anyhow::Result<Option<T>> {
    let Some(value) = value else {
        let message = format!(
            "Windows process verification was not run: required prerequisite \
             {prerequisite} is unavailable for `{test}`"
        );
        if required {
            anyhow::bail!("{message}");
        }
        eprintln!("SKIP: {message}");
        return Ok(None);
    };
    Ok(Some(value))
}

fn process_test_prerequisite_or_skip<T>(
    value: Option<T>,
    prerequisite: &str,
    test: &str,
) -> anyhow::Result<Option<T>> {
    process_test_prerequisite(
        value,
        prerequisite,
        test,
        std::env::var_os(REQUIRE_PROCESS_TESTS_ENV).is_some(),
    )
}

fn utf8_hex(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

async fn wait_for_path(path: &Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(25))).await;
    }
}

async fn assert_terminate_kills_descendant(
    backend: &str,
    python: &str,
    env: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let marker = std::env::temp_dir().join(format!(
        "codex-job-descendant-{backend}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let child_code = format!(
        "import os,pathlib,time; pathlib.Path(bytes.fromhex('{}').decode()).write_text(str(os.getpid())); print('{READY_MARKER}',flush=True); time.sleep(60)",
        utf8_hex(&marker.to_string_lossy())
    );
    let code = format!(
        "import subprocess,sys,time; code=bytes.fromhex('{}').decode(); subprocess.Popen([sys.executable,'-u','-c',code]); time.sleep(60)",
        utf8_hex(&child_code)
    );
    let args = vec!["-u".to_string(), "-c".to_string(), code];
    let spawned = if backend == "pipe" {
        spawn_pipe_process_no_stdin(python, &args, Path::new("."), env, /*arg0*/ &None).await?
    } else {
        spawn_pty_process(
            python,
            &args,
            Path::new("."),
            env,
            /*arg0*/ &None,
            TerminalSize::default(),
        )
        .await?
    };
    let (session, mut output_rx, exit_rx) = combine_spawned_output(spawned);
    wait_for_output_contains(&mut output_rx, READY_MARKER, /*timeout_ms*/ 10_000).await?;
    let descendant = ObservedDescendant::open(std::fs::read_to_string(&marker)?.parse()?)?;
    assert!(
        !descendant.has_exited(),
        "descendant must be live before termination"
    );
    session.request_terminate()?;
    let (_, exit_code) = collect_output_until_exit(output_rx, exit_rx, /*timeout_ms*/ 10_000).await;
    assert_ne!(
        exit_code, -1,
        "{backend} root did not exit after termination"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !descendant.has_exited() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    std::fs::remove_file(marker)?;
    Ok(())
}

async fn assert_normal_exit_preserves_descendant(
    backend: &str,
    python: &str,
    env: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let marker_base = std::env::temp_dir().join(format!(
        "codex-job-natural-exit-{backend}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let ready_marker = marker_base.with_extension("ready");
    let survival_marker = marker_base.with_extension("survived");
    let release_marker = marker_base.with_extension("release");
    let child_code = format!(
        "import os,pathlib,time; pathlib.Path(bytes.fromhex('{}').decode()).write_text(str(os.getpid())); release=pathlib.Path(bytes.fromhex('{}').decode()); deadline=time.time()+20\nwhile not release.exists() and time.time()<deadline: time.sleep(.01)\nif release.exists(): pathlib.Path(bytes.fromhex('{}').decode()).write_text('survived')",
        utf8_hex(&ready_marker.to_string_lossy()),
        utf8_hex(&release_marker.to_string_lossy()),
        utf8_hex(&survival_marker.to_string_lossy())
    );
    let code = format!(
        "import pathlib,subprocess,sys,time; code=bytes.fromhex('{}').decode(); ready=pathlib.Path(bytes.fromhex('{}').decode()); subprocess.Popen([sys.executable,'-u','-c',code],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,creationflags=subprocess.DETACHED_PROCESS|subprocess.CREATE_NEW_PROCESS_GROUP); deadline=time.time()+10\nwhile not ready.exists() and time.time()<deadline: time.sleep(.05)\nsys.exit(0 if ready.exists() else 2)",
        utf8_hex(&child_code),
        utf8_hex(&ready_marker.to_string_lossy())
    );
    let args = vec!["-u".to_string(), "-c".to_string(), code];
    let spawned = if backend == "pipe" {
        spawn_pipe_process_no_stdin(python, &args, Path::new("."), env, /*arg0*/ &None).await?
    } else {
        spawn_pty_process(
            python,
            &args,
            Path::new("."),
            env,
            /*arg0*/ &None,
            TerminalSize::default(),
        )
        .await?
    };
    let (session, output_rx, exit_rx) = combine_spawned_output(spawned);
    let (_, exit_code) = collect_output_until_exit(output_rx, exit_rx, /*timeout_ms*/ 10_000).await;
    assert_eq!(exit_code, 0, "{backend} root did not exit normally");
    let _descendant = ObservedDescendant::open(std::fs::read_to_string(&ready_marker)?.parse()?)?;
    drop(session);
    std::fs::write(&release_marker, "continue after session drop")?;

    let survived = wait_for_path(&survival_marker, Duration::from_secs(10)).await;
    let _ = std::fs::remove_file(ready_marker);
    let _ = std::fs::remove_file(survival_marker);
    let _ = std::fs::remove_file(release_marker);
    assert!(survived, "{backend} descendant did not survive normal exit");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_kills_descendants_for_best_effort_pipe_and_atomic_conpty() -> anyhow::Result<()>
{
    let Some(python) = process_test_prerequisite_or_skip(
        find_python(),
        "Python executable (`python3` or `python`)",
        "terminate_kills_descendants_for_best_effort_pipe_and_atomic_conpty",
    )?
    else {
        return Ok(());
    };
    let env: HashMap<String, String> = std::env::vars().collect();
    assert_terminate_kills_descendant("pipe", &python, &env).await?;
    assert_terminate_kills_descendant("ConPTY", &python, &env).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_exit_preserves_descendants_for_pipe_and_conpty() -> anyhow::Result<()> {
    let Some(python) = process_test_prerequisite_or_skip(
        find_python(),
        "Python executable (`python3` or `python`)",
        "normal_exit_preserves_descendants_for_pipe_and_conpty",
    )?
    else {
        return Ok(());
    };
    let env: HashMap<String, String> = std::env::vars().collect();
    assert_normal_exit_preserves_descendant("pipe", &python, &env).await?;
    assert_normal_exit_preserves_descendant("ConPTY", &python, &env).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conpty_delivers_input_to_foreground_children() -> anyhow::Result<()> {
    let Some(python) = process_test_prerequisite_or_skip(
        find_python(),
        "Python executable (`python3` or `python`)",
        "conpty_delivers_input_to_foreground_children",
    )?
    else {
        return Ok(());
    };
    let code = format!(
        "print('__CODEX_CHILD_'+'READY__', flush=True); value=input(); print('{VALUE_MARKER}'+value.encode('utf-8').hex(), flush=True)"
    );
    let expected = "cafeé 漢字";
    let expected_marker = format!("{VALUE_MARKER}{}", utf8_hex(expected));
    let mut shells = vec![WindowsShell {
        name: "cmd",
        program: std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
        args: vec!["/D".to_string(), "/Q".to_string()],
        child_command: format!("\"{}\" -u -c \"{code}\"", python.replace('"', "\"\"")),
    }];
    if let Some(program) = process_test_prerequisite_or_skip(
        find_powershell(),
        "PowerShell executable (`pwsh.exe` or `powershell.exe`)",
        "PowerShell subcase of conpty_delivers_input_to_foreground_children",
    )? {
        shells.push(WindowsShell {
            name: "PowerShell",
            program,
            args: vec!["-NoLogo".to_string(), "-NoProfile".to_string()],
            child_command: format!("& '{}' -u -c \"{code}\"", python.replace('\'', "''")),
        });
    }
    let env: HashMap<String, String> = std::env::vars().collect();

    for shell in shells {
        let spawned = spawn_pty_process(
            &shell.program,
            &shell.args,
            Path::new("."),
            &env,
            /*arg0*/ &None,
            TerminalSize::default(),
        )
        .await?;
        let (session, mut output_rx, exit_rx) = combine_spawned_output(spawned);
        let writer = session.writer_sender();
        writer
            .send(format!("{}\n", shell.child_command).into_bytes())
            .await?;
        wait_for_output_contains(&mut output_rx, READY_MARKER, /*timeout_ms*/ 10_000)
            .await
            .map_err(|err| anyhow::anyhow!("{} child did not become ready: {err}", shell.name))?;

        writer
            .send(format!("{expected}X\u{8}\n").into_bytes())
            .await?;
        let mut output =
            wait_for_output_contains(&mut output_rx, &expected_marker, /*timeout_ms*/ 10_000)
                .await
                .map_err(|err| {
                    anyhow::anyhow!("{} child received incorrect input: {err}", shell.name)
                })?;

        writer.send(b"exit 0\n".to_vec()).await?;
        let (remaining, exit_code) =
            collect_output_until_exit(output_rx, exit_rx, /*timeout_ms*/ 10_000).await;
        output.extend_from_slice(&remaining);

        assert_eq!(
            exit_code,
            0,
            "{} did not exit cleanly: {:?}",
            shell.name,
            String::from_utf8_lossy(&output)
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conpty_ctrl_c_interrupts_powershell_foreground_child() -> anyhow::Result<()> {
    let Some(program) = process_test_prerequisite_or_skip(
        find_powershell(),
        "PowerShell executable (`pwsh.exe` or `powershell.exe`)",
        "conpty_ctrl_c_interrupts_powershell_foreground_child",
    )?
    else {
        return Ok(());
    };
    let args = vec!["-NoLogo".to_string(), "-NoProfile".to_string()];
    let env: HashMap<String, String> = std::env::vars().collect();
    let spawned = spawn_pty_process(
        &program,
        &args,
        Path::new("."),
        &env,
        /*arg0*/ &None,
        TerminalSize::default(),
    )
    .await?;
    let (session, mut output_rx, exit_rx) = combine_spawned_output(spawned);
    let writer = session.writer_sender();
    writer.send(b"ping.exe -4 -t localhost\n".to_vec()).await?;
    wait_for_output_contains(&mut output_rx, "127.0.0.1", /*timeout_ms*/ 10_000).await?;

    writer.send(vec![0x03]).await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    writer.send(b"cmd.exe /D /C ver\n".to_vec()).await?;
    let mut output = wait_for_output_contains(
        &mut output_rx,
        "Microsoft Windows",
        /*timeout_ms*/ 10_000,
    )
    .await?;

    writer.send(b"exit 0\n".to_vec()).await?;
    let (remaining, exit_code) =
        collect_output_until_exit(output_rx, exit_rx, /*timeout_ms*/ 10_000).await;
    output.extend_from_slice(&remaining);
    assert_eq!(
        exit_code,
        0,
        "PowerShell did not resume after Ctrl-C: {:?}",
        String::from_utf8_lossy(&output)
    );
    Ok(())
}

#[test]
fn required_process_test_prerequisites_report_unverified_coverage() {
    for prerequisite in [
        "Python executable (`python3` or `python`)",
        "PowerShell executable (`pwsh.exe` or `powershell.exe`)",
    ] {
        let err = process_test_prerequisite::<()>(
            None,
            prerequisite,
            "job_object_probe",
            /*required*/ true,
        )
        .expect_err("a missing required prerequisite must fail verification");

        assert_eq!(
            err.to_string(),
            format!(
                "Windows process verification was not run: required prerequisite \
                 {prerequisite} is unavailable for `job_object_probe`"
            )
        );
    }
}

struct ObservedDescendant(OwnedHandle);
impl ObservedDescendant {
    fn open(pid: u32) -> std::io::Result<Self> {
        // SAFETY: OpenProcess returns a new owned handle for observation and panic cleanup.
        let raw = unsafe {
            winapi::um::processthreadsapi::OpenProcess(
                winapi::um::winnt::SYNCHRONIZE | winapi::um::winnt::PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(raw.cast()) }))
    }
    fn has_exited(&self) -> bool {
        // SAFETY: the owned handle has SYNCHRONIZE access.
        unsafe {
            winapi::um::synchapi::WaitForSingleObject(self.0.as_raw_handle().cast(), 0)
                == winapi::um::winbase::WAIT_OBJECT_0
        }
    }
}
impl Drop for ObservedDescendant {
    fn drop(&mut self) {
        if !self.has_exited() {
            // SAFETY: the owned handle has terminate and synchronize access.
            unsafe {
                winapi::um::processthreadsapi::TerminateProcess(self.0.as_raw_handle().cast(), 1);
                winapi::um::synchapi::WaitForSingleObject(self.0.as_raw_handle().cast(), 5_000);
            }
        }
    }
}
