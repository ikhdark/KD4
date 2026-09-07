mod managed_process;
pub mod pipe;
mod process;
pub mod process_group;
pub mod pty;
#[cfg(all(test, windows))]
mod tests;

#[cfg(windows)]
mod win;

#[cfg(windows)]
mod windows_input;

pub const DEFAULT_OUTPUT_BYTES_CAP: usize = 1024 * 1024;

pub use managed_process::ManagedRootAdmissionReclaimerGuard;
pub use managed_process::ManagedRootProcess;
pub use managed_process::ManagedRootReclaimFuture;
pub use managed_process::ManagedRootReclaimHook;
#[cfg(windows)]
pub use managed_process::WINDOWS_CREATE_SUSPENDED;
#[cfg(windows)]
pub use managed_process::WINDOWS_PROCESS_OPERATION_TIMEOUT;
pub use managed_process::install_managed_root_admission_reclaimer;
#[cfg(windows)]
pub use managed_process::run_windows_process_operation;
/// Spawn a non-interactive process using regular pipes for stdin/stdout/stderr.
pub use pipe::spawn_process as spawn_pipe_process;
/// Spawn a non-interactive process using regular pipes, but close stdin immediately.
pub use pipe::spawn_process_no_stdin as spawn_pipe_process_no_stdin;
/// Driver-backed process adapter used by integrations with their own process transport.
pub use process::ProcessDriver;
/// Handle for interacting with a spawned process (PTY or pipe).
pub use process::ProcessHandle;
pub use process::ProcessOutputReceiver;
/// Process signal supported by spawned-process handles.
pub use process::ProcessSignal;
/// Bundle of process handles plus split output and exit receivers returned by spawn helpers.
pub use process::SpawnedProcess;
/// Terminal size in character cells used for PTY spawn and resize operations.
pub use process::TerminalSize;
/// Combine stdout/stderr receivers into a single broadcast receiver.
pub use process::combine_output_receivers;
/// Adapt an externally-driven process into the standard spawned-process handle.
pub use process::spawn_from_driver;
/// Backwards-compatible alias for ProcessHandle.
pub type ExecCommandSession = ProcessHandle;
/// Backwards-compatible alias for SpawnedProcess.
pub type SpawnedPty = SpawnedProcess;
/// Report whether ConPTY is available on this platform (Windows only).
pub use pty::conpty_supported;
/// Spawn a process attached to a PTY for interactive use.
pub use pty::spawn_process as spawn_pty_process;

#[cfg(windows)]
pub use win::JobObject;

#[cfg(windows)]
pub use win::PsuedoCon;

#[cfg(windows)]
pub use win::conpty::RawConPty;

#[cfg(windows)]
pub use windows_input::WindowsTtyInputNormalizer;

/// Adds Windows process arguments without applying MSVCRT escaping to a
/// `cmd.exe /c` script. `cmd.exe` parses its script using its own quoting rules,
/// so the script must be appended as one raw, outer-quoted command line tail.
#[cfg(windows)]
pub fn configure_windows_command_args<S>(
    command: &mut std::process::Command,
    program: &std::ffi::OsStr,
    args: &[S],
) where
    S: AsRef<std::ffi::OsStr>,
{
    use std::os::windows::process::CommandExt;

    let Some(payload_index) = windows_cmd_payload_index(program, args) else {
        command.args(args.iter().map(AsRef::as_ref));
        return;
    };

    command.args(args[..payload_index].iter().map(AsRef::as_ref));
    let payload = args[payload_index].as_ref().to_string_lossy();
    command.raw_arg(format!(r#""{payload}""#));
}

#[cfg(windows)]
pub(crate) fn windows_cmd_payload_index<S>(program: &std::ffi::OsStr, args: &[S]) -> Option<usize>
where
    S: AsRef<std::ffi::OsStr>,
{
    let executable = std::path::Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy();
    if !executable.eq_ignore_ascii_case("cmd") && !executable.eq_ignore_ascii_case("cmd.exe") {
        return None;
    }

    let command_switch = args
        .iter()
        .position(|arg| arg.as_ref().eq_ignore_ascii_case("/c"))?;
    let payload_index = command_switch + 1;
    (payload_index + 1 == args.len()).then_some(payload_index)
}
