#![allow(clippy::unwrap_used)]

// This file is copied from https://github.com/wezterm/wezterm (MIT license).
// Copyright (c) 2018-Present Wez Furlong
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

// Local modifications:
// - Place spawned processes in a Job Object so kill operations terminate the
//   full process tree, while normal root exit preserves background descendants.

use anyhow::Context as _;
use filedescriptor::OwnedHandle;
use portable_pty::Child;
use portable_pty::ChildKiller;
use portable_pty::ExitStatus;
use std::io::Error as IoError;
use std::io::Result as IoResult;
use std::os::windows::io::AsRawHandle;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use winapi::shared::minwindef::DWORD;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::INFINITE;

const WAIT_OBJECT_0_RESULT: DWORD = 0;
const WAIT_FAILED_RESULT: DWORD = u32::MAX;

pub(crate) mod conpty;
mod job;
mod procthreadattr;
mod psuedocon;

pub use conpty::ConPtySystem;
pub use job::JobObject;
pub use psuedocon::PsuedoCon;
pub use psuedocon::conpty_supported;

#[cfg(test)]
thread_local! {
    static DUPLICATION_FAILURE_COUNTDOWN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn clone_process_handle(process: &OwnedHandle) -> IoResult<OwnedHandle> {
    #[cfg(test)]
    if DUPLICATION_FAILURE_COUNTDOWN.with(|remaining| {
        let count = remaining.get();
        remaining.set(count.saturating_sub(1));
        count == 1
    }) {
        return Err(IoError::from_raw_os_error(8));
    }
    process.try_clone().map_err(IoError::other)
}

#[derive(Debug)]
pub struct WinChild {
    proc: Arc<Mutex<OwnedHandle>>,
    job: Arc<JobObject>,
    waiter: Option<tokio::sync::oneshot::Receiver<IoResult<()>>>,
}

impl WinChild {
    pub(crate) fn new(proc: OwnedHandle, job: Arc<JobObject>) -> Self {
        Self {
            proc: Arc::new(Mutex::new(proc)),
            job,
            waiter: None,
        }
    }

    fn is_complete(&mut self) -> IoResult<Option<ExitStatus>> {
        let mut status: DWORD = 0;
        let proc = clone_process_handle(
            &*self
                .proc
                .lock()
                .map_err(|_| IoError::other("process handle lock poisoned"))?,
        )?;
        // A terminating process can publish its exit code before its handle is
        // signaled. Only the native wait proves that the process has exited.
        // SAFETY: proc is an owned duplicate of the process handle and remains live for the
        // entire zero-duration wait.
        match unsafe { WaitForSingleObject(proc.as_raw_handle() as _, 0) } {
            winapi::shared::winerror::WAIT_TIMEOUT => return Ok(None),
            WAIT_FAILED_RESULT => return Err(IoError::last_os_error()),
            WAIT_OBJECT_0_RESULT => {}
            result => {
                return Err(IoError::other(format!(
                    "unexpected process wait result: 0x{result:08x}"
                )));
            }
        }
        // SAFETY: proc owns a live process handle and status is writable DWORD storage for the
        // exit-code output.
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            self.preserve_descendants();
            Ok(Some(ExitStatus::with_exit_code(status)))
        } else {
            Err(IoError::last_os_error())
        }
    }

    fn do_kill(&mut self) -> IoResult<()> {
        terminate_job_or_process(&self.job, self.proc.as_ref())
    }

    fn preserve_descendants(&self) {
        if let Err(err) = self.job.preserve_descendants() {
            log::warn!("ConPTY failed to preserve descendants after root exit: {err}");
        }
    }
}

impl ChildKiller for WinChild {
    fn kill(&mut self) -> IoResult<()> {
        self.do_kill()
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(WinChildKiller {
            job: Arc::clone(&self.job),
            proc: Arc::clone(&self.proc),
        })
    }
}

#[derive(Debug)]
pub struct WinChildKiller {
    job: Arc<JobObject>,
    proc: Arc<Mutex<OwnedHandle>>,
}

impl ChildKiller for WinChildKiller {
    fn kill(&mut self) -> IoResult<()> {
        terminate_job_or_process(&self.job, self.proc.as_ref())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(WinChildKiller {
            job: Arc::clone(&self.job),
            proc: Arc::clone(&self.proc),
        })
    }
}

fn terminate_process(process: &Mutex<OwnedHandle>) -> IoResult<()> {
    let process = process
        .lock()
        .map_err(|_| IoError::other("process handle lock poisoned"))?;
    // SAFETY: The mutex guard keeps the owned process handle live and prevents replacement
    // until TerminateProcess returns.
    let terminated = unsafe { TerminateProcess(process.as_raw_handle() as _, 1) };
    if terminated == 0 {
        Err(IoError::last_os_error())
    } else {
        Ok(())
    }
}

fn terminate_job_or_process(job: &JobObject, process: &Mutex<OwnedHandle>) -> IoResult<()> {
    match job.terminate() {
        Ok(()) => Ok(()),
        Err(job_err) => {
            log::warn!(
                "ConPTY failed to terminate process tree; terminating root process: {job_err}"
            );
            terminate_process(process).map_err(|process_err| {
                IoError::other(format!(
                    "failed to terminate ConPTY job ({job_err}); root process fallback also \
                     failed: {process_err}"
                ))
            })
        }
    }
}

impl Child for WinChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        self.is_complete()
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }
        let proc = clone_process_handle(
            &*self
                .proc
                .lock()
                .map_err(|_| IoError::other("process handle lock poisoned"))?,
        )?;
        // SAFETY: proc owns the duplicated process handle throughout the wait, so it cannot be
        // closed concurrently.
        let wait_result = unsafe { WaitForSingleObject(proc.as_raw_handle() as _, INFINITE) };
        if wait_result == WAIT_FAILED_RESULT {
            return Err(IoError::last_os_error());
        }
        if wait_result != WAIT_OBJECT_0_RESULT {
            return Err(IoError::other(format!(
                "unexpected process wait result: 0x{wait_result:08x}"
            )));
        }
        let mut status: DWORD = 0;
        // SAFETY: proc remains owned and status is writable DWORD storage for the duration of
        // GetExitCodeProcess.
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            self.preserve_descendants();
            Ok(ExitStatus::with_exit_code(status))
        } else {
            Err(IoError::last_os_error())
        }
    }

    fn process_id(&self) -> Option<u32> {
        // SAFETY: The mutex guard keeps the owned process handle live while GetProcessId reads
        // its process identity.
        let res = unsafe { GetProcessId(self.proc.lock().unwrap().as_raw_handle() as _) };
        if res == 0 { None } else { Some(res) }
    }

    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        let proc = self.proc.lock().unwrap();
        Some(proc.as_raw_handle())
    }
}

impl std::future::Future for WinChild {
    type Output = anyhow::Result<ExitStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<anyhow::Result<ExitStatus>> {
        match self.is_complete() {
            Ok(Some(status)) => Poll::Ready(Ok(status)),
            Err(err) => Poll::Ready(Err(err).context("Failed to retrieve process exit status")),
            Ok(None) => {
                if self.waiter.is_none() {
                    let proc = self
                        .proc
                        .lock()
                        .map_err(|_| IoError::other("process handle lock poisoned"))?
                        .try_clone()?;
                    let (sender, receiver) = tokio::sync::oneshot::channel();
                    std::thread::Builder::new()
                        .name("codex-process-wait".into())
                        .spawn(move || {
                            let result =
                                // SAFETY: The waiter thread owns proc until this wait
                                // completes; no other owner closes that duplicated handle.
                                unsafe { WaitForSingleObject(proc.as_raw_handle() as _, INFINITE) };
                            let result = if result == WAIT_OBJECT_0_RESULT {
                                Ok(())
                            } else if result == WAIT_FAILED_RESULT {
                                Err(IoError::last_os_error())
                            } else {
                                Err(IoError::other(format!(
                                    "unexpected process wait result: 0x{result:08x}"
                                )))
                            };
                            let _ = sender.send(result);
                        })?;
                    self.waiter = Some(receiver);
                }
                // oneshot replaces the registered waker on each poll. Repolling
                // the child never creates another native waiter thread.
                let receiver = self.waiter.as_mut().expect("waiter initialized above");
                match std::future::Future::poll(Pin::new(receiver), cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => {
                        result??;
                        Poll::Ready(self.is_complete()?.ok_or_else(|| {
                            anyhow::anyhow!("process remained active after wait was signaled")
                        }))
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod waiter_tests {
    use super::*;
    use std::future::Future;
    use std::os::windows::io::FromRawHandle;
    use std::process::Stdio;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Wake;
    use std::task::Waker;
    use std::time::Duration;

    struct ChildCleanup(std::process::Child);
    impl Drop for ChildCleanup {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    struct WakeCount(AtomicUsize);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn native_conpty_status_and_wait_return_duplication_errors_without_losing_child()
    -> anyhow::Result<()> {
        use std::os::windows::io::BorrowedHandle;
        let (console, input, output) = conpty::RawConPty::new(80, 24)?.into_handles();
        let mut command = portable_pty::CommandBuilder::new("cmd.exe");
        command.args(["/D", "/Q", "/K"]);
        let mut child = console.spawn_command(command)?;
        let native = {
            let process = child.proc.lock().unwrap();
            // SAFETY: The process mutex guard keeps the original handle alive until it has been
            // duplicated into a separate OwnedHandle.
            unsafe { BorrowedHandle::borrow_raw(process.as_raw_handle()) }.try_clone_to_owned()?
        };
        assert_eq!(
            unsafe { WaitForSingleObject(native.as_raw_handle() as _, 0) },
            winapi::shared::winerror::WAIT_TIMEOUT
        );
        for fail_on_clone in [1, 2] {
            DUPLICATION_FAILURE_COUNTDOWN.with(|remaining| remaining.set(fail_on_clone));
            let error = if fail_on_clone == 1 {
                child
                    .try_wait()
                    .expect_err("status duplication failure must return an error")
            } else {
                child
                    .wait()
                    .expect_err("native wait handle duplication failure must return an error")
            };
            assert_eq!(error.raw_os_error(), Some(8));
            assert_eq!(
                DUPLICATION_FAILURE_COUNTDOWN.with(std::cell::Cell::get),
                0,
                "normal status/wait must reach the selected external duplication boundary"
            );
            assert_eq!(
                unsafe { WaitForSingleObject(native.as_raw_handle() as _, 0) },
                winapi::shared::winerror::WAIT_TIMEOUT,
                "reporting a handle error must not terminate the actual child"
            );
            assert!(
                child.try_wait()?.is_none(),
                "retry must still observe the running native child"
            );
        }
        child.kill()?;
        assert_eq!(child.wait()?.exit_code(), 1);
        assert_eq!(
            unsafe { WaitForSingleObject(native.as_raw_handle() as _, 0) },
            WAIT_OBJECT_0_RESULT
        );
        drop(child);
        // Once signaled, 259 is a real exit code, not the STILL_ACTIVE sentinel.
        let mut command = portable_pty::CommandBuilder::new("cmd.exe");
        command.args(["/D", "/Q", "/C", "exit 259"]);
        let mut exited = console.spawn_command(command)?;
        assert_eq!(exited.wait()?.exit_code(), 259);
        assert_eq!(
            exited
                .try_wait()?
                .expect("native exit observed")
                .exit_code(),
            259
        );
        drop(exited);
        drop(input);
        drop(output);
        drop(console);
        Ok(())
    }

    #[test]
    fn repeated_child_polls_wake_only_latest_waiter_once() -> anyhow::Result<()> {
        // cmd waits on its open stdin pipe without launching any descendants.
        let mut process = ChildCleanup(
            std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "set /p value="])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        // SAFETY: OpenProcess receives a scalar PID and access mask; its result is checked
        // before ownership is assumed.
        let raw = unsafe {
            OpenProcess(
                winapi::um::winnt::PROCESS_QUERY_INFORMATION
                    | winapi::um::winnt::PROCESS_SET_QUOTA
                    | winapi::um::winnt::PROCESS_TERMINATE
                    | winapi::um::winnt::SYNCHRONIZE,
                0,
                process.0.id(),
            )
        };
        assert!(!raw.is_null());
        // SAFETY: The checked, non-null result of OpenProcess is transferred into its sole
        // owning handle wrapper.
        let owned = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
        let job = Arc::new(JobObject::create()?);
        job.assign_process(owned.as_raw_handle())?;
        let mut child = WinChild::new(owned, job);
        let old = Arc::new(WakeCount(AtomicUsize::new(0)));
        let latest = Arc::new(WakeCount(AtomicUsize::new(0)));
        let old_waker = Waker::from(Arc::clone(&old));
        let latest_waker = Waker::from(Arc::clone(&latest));
        assert!(
            Pin::new(&mut child)
                .poll(&mut Context::from_waker(&old_waker))
                .is_pending()
        );
        for _ in 0..32 {
            assert!(
                Pin::new(&mut child)
                    .poll(&mut Context::from_waker(&latest_waker))
                    .is_pending()
            );
        }
        child.kill()?;
        process.0.wait()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while latest.0.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(old.0.load(Ordering::SeqCst), 0);
        assert_eq!(latest.0.load(Ordering::SeqCst), 1);
        let result = Pin::new(&mut child).poll(&mut Context::from_waker(&latest_waker));
        let Poll::Ready(result) = result else {
            panic!("terminated child must complete")
        };
        assert_eq!(result?.exit_code(), 1);
        Ok(())
    }
}
