//! Live Windows stdio identity, not client-supplied application metadata.

use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::*;

#[derive(Clone, Copy, Debug)]
pub struct StdioPeer {
    pub backend_creation_time: u64,
    pub input_handle: u64,
    pub output_handle: u64,
    pub peer_pid: u32,
    pub peer_creation_time: u64,
}

static STDIO_PEER: OnceLock<Option<StdioPeer>> = OnceLock::new();

/// Run before the native stdio workers: querying a synchronous input handle
/// after its reader starts can wait behind an indefinitely pending ReadFile.
pub fn initialize_stdio_peer() -> io::Result<()> {
    let result = capture_stdio_peer();
    let _ = STDIO_PEER.set(result.as_ref().ok().copied());
    result.map(|_| ())
}

pub fn stdio_peer() -> Option<StdioPeer> {
    STDIO_PEER.get().copied().flatten()
}

fn process(pid: u32, duplicate: bool) -> io::Result<OwnedHandle> {
    let access = PROCESS_QUERY_LIMITED_INFORMATION
        | PROCESS_SYNCHRONIZE
        | if duplicate { PROCESS_DUP_HANDLE } else { 0 };
    // OwnedHandle closes only the successfully opened handle, never a pseudo handle.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn creation_time(process: &OwnedHandle) -> io::Result<u64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } != WAIT_TIMEOUT {
        return Err(io::Error::other("stdio process has exited"));
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn image(process: &OwnedHandle) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 32768];
    let mut size = buffer.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(process.as_raw_handle(), 0, buffer.as_mut_ptr(), &mut size)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buffer[..size as usize],
    )))
}

fn pipe_peer(handle: HANDLE, backend_pid: u32) -> io::Result<u32> {
    let mut server = 0;
    let mut client = 0;
    // Anonymous pipes and libuv named pipes can expose different endpoint roles.
    // Require an OS-reported, non-backend peer; never fall back to ancestry.
    if unsafe { GetNamedPipeServerProcessId(handle, &mut server) } == 0
        || unsafe { GetNamedPipeClientProcessId(handle, &mut client) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut peers = [server, client]
        .into_iter()
        .filter(|pid| *pid != 0 && *pid != backend_pid);
    let peer = peers
        .next()
        .ok_or_else(|| io::Error::other("stdio pipe peer is unavailable"))?;
    if peers.any(|other| other != peer) {
        return Err(io::Error::other("stdio pipe peer is ambiguous"));
    }
    Ok(peer)
}

/// Capture handles only for the transport owner's actual stdio connection.
/// This is advisory until a consumer rechecks the live handles and both processes.
fn capture_stdio_peer() -> io::Result<StdioPeer> {
    let pid = std::process::id();
    let backend = process(pid, false)?;
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let peer_pid = pipe_peer(input, pid)?;
    if pipe_peer(output, pid)? != peer_pid {
        return Err(io::Error::other("stdio input/output peers differ"));
    }
    let peer = process(peer_pid, false)?;
    Ok(StdioPeer {
        backend_creation_time: creation_time(&backend)?,
        input_handle: input as usize as u64,
        output_handle: output as usize as u64,
        peer_pid,
        peer_creation_time: creation_time(&peer)?,
    })
}

fn duplicate(process: &OwnedHandle, handle: u64) -> io::Result<OwnedHandle> {
    let value = usize::try_from(handle).map_err(|_| io::Error::other("invalid stdio handle"))?;
    let mut copied = std::ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            process.as_raw_handle(),
            value as HANDLE,
            GetCurrentProcess(),
            &mut copied,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(copied) })
}

fn matches_path(actual: &Path, expected: &Path) -> bool {
    let actual = actual
        .canonicalize()
        .unwrap_or_else(|_| actual.to_path_buf());
    let expected = expected
        .canonicalize()
        .unwrap_or_else(|_| expected.to_path_buf());
    actual
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.to_string_lossy())
}

/// Recheck the live output peer, pinned input handle, executable paths and
/// creation identities. No pipe data is read or written.
pub fn verify_stdio_peer(
    pid: u32,
    binding: StdioPeer,
    backend_path: &Path,
    gui_path: &Path,
) -> io::Result<()> {
    let backend = process(pid, true)?;
    let peer = process(binding.peer_pid, false)?;
    if creation_time(&backend)? != binding.backend_creation_time
        || creation_time(&peer)? != binding.peer_creation_time
    {
        return Err(io::Error::other(
            "live stdio process identity does not match the selected GUI/backend",
        ));
    }
    // Duplicate the input identity, but do not issue synchronous I/O controls
    // against it while the transport reader can hold a pending ReadFile. The
    // transport pins this original input for the entire connection lifetime.
    let _input = duplicate(&backend, binding.input_handle)?;
    let output = duplicate(&backend, binding.output_handle)?;
    if pipe_peer(output.as_raw_handle(), pid)? != binding.peer_pid {
        return Err(io::Error::other(
            "live stdio peer does not match the receipt",
        ));
    }
    if !matches_path(&image(&backend)?, backend_path)
        || !matches_path(&image(&peer)?, gui_path)
    {
        return Err(io::Error::other(
            "live stdio executable does not match the selected GUI/backend",
        ));
    }
    creation_time(&backend)?;
    creation_time(&peer)?;
    Ok(())
}
