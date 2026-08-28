use super::*;
use std::collections::HashMap;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::BorrowedHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::os::windows::process::CommandExt;
use std::sync::Arc;
use std::time::Duration;
use winapi::um::processthreadsapi::OpenProcess;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::WAIT_OBJECT_0;
use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;
use winapi::um::winnt::SYNCHRONIZE;

#[test]
fn managed_job_terminates_root() -> anyhow::Result<()> {
    let managed = Arc::new(ManagedRootProcess::reserve()?);
    let mut command = std::process::Command::new("ping.exe");
    command
        .args(["-n", "60", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    managed.attach(child.id())?;
    let process =
        unsafe { BorrowedHandle::borrow_raw(child.as_raw_handle()) }.try_clone_to_owned()?;
    let mut terminator = PipeChildTerminator {
        managed,
        windows: WindowsChildTerminator::Job { process },
    };

    terminator.kill()?;

    assert!(!child.wait()?.success());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn managed_job_terminates_child_and_grandchild() -> anyhow::Result<()> {
    let args = vec![
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-Command".to_string(),
        "$child = Start-Process ping.exe -ArgumentList '-n','60','127.0.0.1' -PassThru; \
         [Console]::Out.WriteLine($child.Id); [Console]::Out.Flush(); Start-Sleep -Seconds 60"
            .to_string(),
    ];
    let cwd = std::env::current_dir()?;
    let env = std::env::vars().collect::<HashMap<_, _>>();
    let spawned = spawn_process("powershell.exe", &args, &cwd, &env, &None).await?;
    let SpawnedProcess {
        session,
        mut stdout_rx,
        exit_rx,
        ..
    } = spawned;

    let output = tokio::time::timeout(Duration::from_secs(10), stdout_rx.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("managed root closed stdout before reporting child pid"))?;
    let grandchild_pid = std::str::from_utf8(&output)?.trim().parse::<u32>()?;

    session.terminate().expect("terminate pipe process");
    let _ = tokio::time::timeout(Duration::from_secs(5), exit_rx).await?;

    let raw = unsafe {
        OpenProcess(
            SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            grandchild_pid,
        )
    };
    if !raw.is_null() {
        let process = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
        let wait = unsafe { WaitForSingleObject(process.as_raw_handle() as _, 2_000) };
        assert_eq!(
            wait, WAIT_OBJECT_0,
            "grandchild remained alive after Job termination"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn suspended_root_waits_for_job_assignment_before_running() -> anyhow::Result<()> {
    let marker = std::env::temp_dir().join(format!(
        "codex-suspended-root-{}-{}.marker",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let marker_literal = marker.to_string_lossy().replace('\'', "''");
    let managed = ManagedRootProcess::reserve()?;
    let mut command = std::process::Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("[IO.File]::WriteAllText('{marker_literal}', 'ran')"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(WINDOWS_CREATE_SUSPENDED);
    let mut child = command.spawn()?;

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !marker.exists(),
        "suspended child ran before Job assignment"
    );

    managed.attach_and_resume(child.id())?;
    let status = child.wait()?;
    assert!(status.success());
    assert!(marker.exists(), "child did not run after it was resumed");
    std::fs::remove_file(marker)?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn windows_process_spawn_timeout_does_not_block_async_runtime() {
    let error = run_windows_process_operation(Duration::from_millis(20), || {
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    })
    .await
    .expect_err("the blocking spawn operation should time out");

    assert_eq!(error.kind(), ErrorKind::TimedOut);
}
