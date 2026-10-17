//! The host must not open a console window when its parent has no console
//! (validation T10), and must share its parent's console when there is one,
//! which spawns about 5 ms faster than a hidden console of its own.
//!
//! One test per binary: it detaches this test process from its console.
#![cfg(windows)]
#![allow(clippy::expect_used)]

use std::os::windows::process::CommandExt;
use std::sync::Arc;

use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::NoopCodeModeSessionDelegate;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use windows_sys::Win32::System::Console::AttachConsole;
use windows_sys::Win32::System::Console::FreeConsole;
use windows_sys::Win32::System::Console::GetConsoleProcessList;
use windows_sys::Win32::System::Console::GetConsoleWindow;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn child_host_pids() -> Vec<u32> {
    let filter = format!(
        "ParentProcessId={} AND Name='codex-code-mode-host.exe'",
        std::process::id()
    );
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("Get-CimInstance Win32_Process -Filter \"{filter}\" | ForEach-Object {{ $_.ProcessId }}"),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("list host processes");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

/// Processes attached to this process's console; empty without one.
fn console_process_ids() -> Vec<u32> {
    let mut pids = vec![0_u32; 256];
    // SAFETY: the buffer holds the number of u32 values its length declares.
    let count = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    pids.truncate(count as usize);
    pids
}

struct HostConsole {
    window: *mut std::ffi::c_void,
    process_ids: Vec<u32>,
}

/// Attaches to the console of the one host started since `before`, leaving
/// this process without a console.
fn new_host_console(before: &[u32]) -> HostConsole {
    let started: Vec<u32> = child_host_pids()
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .collect();
    assert_eq!(
        started.len(),
        1,
        "expected exactly one new host child: {started:?}"
    );
    // SAFETY: FreeConsole, AttachConsole and GetConsoleWindow take plain
    // values and touch only this process's console attachment.
    unsafe {
        FreeConsole();
        if AttachConsole(started[0]) == 0 {
            return HostConsole {
                window: std::ptr::null_mut(),
                process_ids: Vec::new(),
            };
        }
        let console = HostConsole {
            window: GetConsoleWindow(),
            process_ids: console_process_ids(),
        };
        FreeConsole();
        console
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_shares_a_parent_console_and_opens_none_without_one() {
    let program = codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary");
    let this_process = std::process::id();
    let console_peers: Vec<u32> = console_process_ids()
        .into_iter()
        .filter(|pid| *pid != this_process)
        .collect();
    if console_peers.is_empty() {
        eprintln!("skipping the shared-console check: no other process shares this console");
    } else {
        let before = child_host_pids();
        let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(program.clone());
        let session = provider
            .create_session(Arc::new(NoopCodeModeSessionDelegate))
            .await
            .expect("create session");
        let console = new_host_console(&before);
        assert!(
            console
                .process_ids
                .iter()
                .any(|pid| console_peers.contains(pid)),
            "the host must share its parent's console: {:?} vs {console_peers:?}",
            console.process_ids
        );
        session.shutdown().await.expect("shutdown");
    }

    // Now detached like a console-less Desktop app-server. Windows gives a
    // console program started this way a new, visible console window unless
    // the spawn suppresses it.
    // SAFETY: FreeConsole takes no arguments; this binary runs no other test.
    unsafe { FreeConsole() };
    let before = child_host_pids();
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(program);
    let session = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .expect("create session");
    let console = new_host_console(&before);
    assert!(
        console.window.is_null(),
        "the host owns a console window: {:?}",
        console.window
    );
    session.shutdown().await.expect("shutdown");
}
