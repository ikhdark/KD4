use super::*;
use std::collections::HashMap;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::BorrowedHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::os::windows::process::CommandExt;
use std::sync::Arc;
use std::time::Duration;
use winapi::shared::winerror::WAIT_TIMEOUT;
use winapi::um::processthreadsapi::OpenProcess;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::WAIT_OBJECT_0;
use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;
use winapi::um::winnt::SYNCHRONIZE;

#[tokio::test(flavor = "current_thread")]
async fn pipe_setup_native_assignment_failure_reaps_child_before_returning_error()
-> anyhow::Result<()> {
    let mut managed = ManagedRootProcess::reserve()?;
    managed.restrict_job_to_query_access_for_test()?;
    let mut command = tokio::process::Command::new("cmd.exe");
    command
        .args(["/D", "/Q", "/K"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(WINDOWS_CREATE_SUSPENDED)
        .kill_on_drop(true);
    let child = command.spawn()?;
    let process =
        unsafe { BorrowedHandle::borrow_raw(child.raw_handle().unwrap()) }.try_clone_to_owned()?;
    assert_eq!(
        unsafe { WaitForSingleObject(process.as_raw_handle() as _, 0) },
        WAIT_TIMEOUT
    );

    // This is the normal setup boundary called immediately after public pipe creation.
    // Only its native Job resource has reduced rights; child, assignment, kill and wait are real.
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        finish_pipe_process_setup(child, managed),
    )
    .await?;
    let error = match result {
        Ok(_) => anyhow::bail!("query-only Job unexpectedly admitted the pipe process"),
        Err(error) => error,
    };
    let native = error
        .downcast_ref::<io::Error>()
        .expect("preserved assignment error");
    assert_eq!(
        native.raw_os_error(),
        Some(5),
        "native ACCESS_DENIED must survive cleanup"
    );
    assert_eq!(
        unsafe { WaitForSingleObject(process.as_raw_handle() as _, 0) },
        WAIT_OBJECT_0,
        "normal setup must confirm native exit before returning its assignment error",
    );
    Ok(())
}

#[test]
fn pipe_setup_deadline_retains_native_child_until_cleanup_worker_runs() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .max_blocking_threads(1)
        .build()?;
    runtime.block_on(async {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let occupied = tokio::task::spawn_blocking(move || {
            let _ = entered_tx.send(());
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        entered_rx.await?;
        let mut managed = ManagedRootProcess::reserve()?;
        managed.restrict_job_to_query_access_for_test()?;
        let child = tokio::process::Command::new("cmd.exe")
            .args(["/D", "/Q", "/K"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(WINDOWS_CREATE_SUSPENDED)
            .kill_on_drop(true)
            .spawn()?;
        let process = unsafe { BorrowedHandle::borrow_raw(child.raw_handle().unwrap()) }
            .try_clone_to_owned()?;
        let started = tokio::time::Instant::now();
        let mut setup = Box::pin(finish_pipe_process_setup(child, managed));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(setup.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::advance(Duration::from_secs(5)).await;
        let result = setup.await;
        let error = match result {
            Ok(_) => anyhow::bail!("query-only Job unexpectedly admitted the pipe process"),
            Err(error) => error,
        };
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
            Some(5)
        );
        assert!(started.elapsed() >= Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(6));
        assert_eq!(
            unsafe { WaitForSingleObject(process.as_raw_handle() as _, 0) },
            WAIT_TIMEOUT,
            "timed-out caller must leave the queued cleanup owner in custody of the live child",
        );
        release_tx.send(())?;
        occupied.await?;
        tokio::time::resume();
        tokio::time::timeout(Duration::from_secs(5), async {
            while unsafe { WaitForSingleObject(process.as_raw_handle() as _, 0) } != WAIT_OBJECT_0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(())
    })
}

#[tokio::test(flavor = "current_thread")]
async fn public_pipe_handle_duplication_failure_reaps_native_child() -> anyhow::Result<()> {
    let witnessed_process = Arc::new(StdMutex::new(None));
    let capture = Arc::clone(&witnessed_process);
    TEST_DUPLICATE_PROCESS_HANDLE.with(|duplicate| {
        *duplicate.borrow_mut() = Some(Box::new(move |process| {
            let owned = unsafe { BorrowedHandle::borrow_raw(process) }.try_clone_to_owned()?;
            assert_eq!(
                unsafe { WaitForSingleObject(owned.as_raw_handle() as _, 0) },
                WAIT_TIMEOUT,
                "native child must be alive before external DuplicateHandle failure",
            );
            *capture.lock().unwrap() = Some(owned);
            Err(io::Error::from_raw_os_error(8))
        }));
    });
    let result = spawn_process(
        "cmd.exe",
        &["/D".to_string(), "/Q".to_string(), "/K".to_string()],
        &std::env::current_dir()?,
        &std::env::vars().collect(),
        &None,
    )
    .await;
    let error = match result {
        Ok(_) => anyhow::bail!("failed duplicate unexpectedly published a process session"),
        Err(error) => error,
    };
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
        Some(8)
    );
    let process = witnessed_process
        .lock()
        .unwrap()
        .take()
        .expect("normal process setup reached duplication");
    assert_eq!(
        unsafe { WaitForSingleObject(process.as_raw_handle() as _, 0) },
        WAIT_OBJECT_0,
        "failed public setup must reap the actual child without publishing a session",
    );
    assert!(TEST_DUPLICATE_PROCESS_HANDLE.with(|duplicate| duplicate.borrow().is_none()));
    Ok(())
}

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
        "$child = Start-Process ping.exe -WindowStyle Hidden -ArgumentList '-n','60','127.0.0.1' -PassThru; \
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

    let raw = unsafe {
        OpenProcess(
            SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            grandchild_pid,
        )
    };
    anyhow::ensure!(
        !raw.is_null(),
        "could not open the running grandchild: {}",
        std::io::Error::last_os_error()
    );
    let process = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    assert_eq!(
        unsafe { WaitForSingleObject(process.as_raw_handle() as _, 0) },
        WAIT_TIMEOUT,
        "grandchild must be alive before termination"
    );

    session.terminate().expect("terminate pipe process");
    let exit_code = tokio::time::timeout(Duration::from_secs(5), exit_rx).await??;
    assert_ne!(exit_code, 0, "terminated root must report failure");
    assert!(session.has_exited());
    assert_eq!(session.exit_code(), Some(exit_code));
    let wait = unsafe { WaitForSingleObject(process.as_raw_handle() as _, 2_000) };
    assert_eq!(
        wait, WAIT_OBJECT_0,
        "grandchild remained alive after Job termination"
    );
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
