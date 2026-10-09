//! The parser must not keep unrelated output pipes alive. Windows' ordinary
//! `Command::spawn` inherits all inheritable handles, even with redirected stdio.
//! Keep this fixed-purpose launcher local until std exposes a stable handle list.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::ptr;
use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
use windows_sys::Win32::Foundation::DuplicateHandle;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
use windows_sys::Win32::Globalization::CompareStringOrdinal;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
use windows_sys::Win32::System::Threading::CreateProcessW;
use windows_sys::Win32::System::Threading::DeleteProcThreadAttributeList;
use windows_sys::Win32::System::Threading::EXTENDED_STARTUPINFO_PRESENT;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::InitializeProcThreadAttributeList;
use windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST;
use windows_sys::Win32::System::Threading::PROC_THREAD_ATTRIBUTE_HANDLE_LIST;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
use windows_sys::Win32::System::Threading::STARTUPINFOEXW;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::UpdateProcThreadAttribute;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

pub(super) struct Child {
    process: OwnedHandle,
    pub(super) stdin: Option<File>,
    pub(super) stdout: Option<File>,
}

impl Child {
    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_for(0)
    }

    pub(super) fn wait(&mut self) -> io::Result<Option<ExitStatus>> {
        drop(self.stdin.take());
        self.wait_for(INFINITE)
    }

    fn wait_for(&self, milliseconds: u32) -> io::Result<Option<ExitStatus>> {
        // SAFETY: process owns a live process handle throughout both calls.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), milliseconds) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: code is writable output storage; the process handle is owned.
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(ExitStatus::from_raw(code)))
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub(super) fn kill(&mut self) -> io::Result<()> {
        // SAFETY: termination uses the owned handle, never a potentially reused process id.
        if unsafe { TerminateProcess(self.process.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "embedded NUL in parser launch",
        ));
    }
    value.push(0);
    Ok(value)
}

fn environment() -> io::Result<Vec<u16>> {
    environment_block(std::env::vars_os())
}

fn environment_block(entries: impl Iterator<Item = (OsString, OsString)>) -> io::Result<Vec<u16>> {
    let mut entries = entries
        .filter(|(key, _)| {
            !key.to_string_lossy()
                .eq_ignore_ascii_case(super::PARSER_SOURCE_ENV)
        })
        .chain(std::iter::once((
            OsString::from(super::PARSER_SOURCE_ENV),
            OsString::from(super::encoded_parser_source()),
        )))
        .map(|(key, value)| Ok((wide(&key)?, wide(&value)?)))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort_by(|(a, _), (b, _)| {
        // SAFETY: both keys are valid NUL-terminated UTF-16 buffers. Windows
        // environment blocks use case-insensitive ordinal ordering, not locale collation.
        match unsafe { CompareStringOrdinal(a.as_ptr(), -1, b.as_ptr(), -1, 1) } {
            1 => Ordering::Less,
            3 => Ordering::Greater,
            _ => Ordering::Equal,
        }
    });
    let mut block = Vec::new();
    for (mut key, value) in entries {
        let _ = key.pop();
        block.extend(key);
        block.push(u16::from(b'='));
        block.extend(value);
    }
    block.push(0);
    Ok(block)
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    // SAFETY: output pointers are valid; null security attributes make both
    // handles non-inheritable. Only explicitly selected duplicates are inherited.
    if unsafe { CreatePipe(&mut read, &mut write, ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreatePipe transfers ownership of two distinct handles.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}

fn inheritable(handle: &impl AsRawHandle) -> io::Result<OwnedHandle> {
    let mut duplicate = ptr::null_mut();
    // SAFETY: source is borrowed for the call; the current-process pseudo handle
    // needs no cleanup. DuplicateHandle creates a separately owned local handle.
    let result = unsafe {
        let current = GetCurrentProcess();
        DuplicateHandle(
            current,
            handle.as_raw_handle(),
            current,
            &mut duplicate,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful DuplicateHandle transfers ownership of duplicate.
    Ok(unsafe { OwnedHandle::from_raw_handle(duplicate) })
}

struct Attributes(Vec<usize>);

impl Attributes {
    fn new() -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: null requests only the required size in writable storage.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        // usize storage provides pointer alignment as well as the requested size.
        let mut storage = vec![0; bytes.div_ceil(size_of::<usize>())];
        // SAFETY: storage is writable, aligned and covers the API-reported size.
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut bytes)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(storage))
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: Attributes is constructed only after initialization succeeds.
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

pub(super) fn spawn(executable: &Path, cwd: &Path) -> io::Result<Child> {
    // Match std's native path normalization. Legacy Windows PowerShell's .NET
    // bootstrap cannot initialize from a verbatim drive path, even though
    // CreateProcessW itself accepts it. Simplify only semantically safe prefixes.
    let executable = codex_utils_absolute_path::normalize_for_native_workdir(executable);
    let cwd = codex_utils_absolute_path::normalize_for_native_workdir(cwd);
    let application = wide(executable.as_os_str())?;
    let directory = wide(cwd.as_os_str())?;
    // Executable is a canonical trusted filesystem path, not arbitrary argv;
    // Windows filenames cannot contain a quote. All other arguments are fixed or base64.
    let mut command = OsString::from("\"");
    command.push(&executable);
    command.push("\" -NoLogo -NoProfile -NonInteractive -EncodedCommand ");
    command.push(super::encoded_parser_bootstrap());
    let mut command = wide(&command)?;
    let environment = environment()?;
    let (stdin_read, stdin_write) = pipe()?;
    let (stdout_read, stdout_write) = pipe()?;
    let stdin = inheritable(&stdin_read)?;
    let stdout = inheritable(&stdout_write)?;
    let stderr = inheritable(&File::options().write(true).open("NUL")?)?;
    let handles = [
        stdin.as_raw_handle(),
        stdout.as_raw_handle(),
        stderr.as_raw_handle(),
    ];
    let mut attributes = Attributes::new()?;
    // SAFETY: handles remains alive and unchanged through CreateProcessW and
    // contains only valid inheritable handles; the initialized list is owned.
    if unsafe {
        UpdateProcThreadAttribute(
            attributes.as_mut_ptr(),
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            size_of_val(&handles),
            ptr::null_mut(),
            ptr::null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = attributes.as_mut_ptr();
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: all native strings are NUL-terminated, command is writable, the
    // environment is double-NUL-terminated, and the startup/list/handles outlive
    // this synchronous call. No caller-owned handle flags or global state change.
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            environment.as_ptr().cast(),
            directory.as_ptr(),
            &startup.StartupInfo,
            &mut process,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreateProcessW transfers these two distinct handles.
    let (process, thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(process.hProcess),
            OwnedHandle::from_raw_handle(process.hThread),
        )
    };
    drop(thread);
    Ok(Child {
        process,
        stdin: Some(stdin_write.into()),
        stdout: Some(stdout_read.into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn environment_block_orders_names_not_values() -> io::Result<()> {
        let block = environment_block(
            [
                (OsString::from("A0"), OsString::from("second")),
                (OsString::from("A"), OsString::from("first")),
            ]
            .into_iter(),
        )?;
        assert!(block.starts_with(&"A=first\0A0=second\0".encode_utf16().collect::<Vec<_>>()));
        assert!(block.ends_with(&[0, 0]));
        Ok(())
    }

    #[test]
    fn missing_executable_fails_after_native_handle_setup() -> anyhow::Result<()> {
        let current = std::env::current_exe()?;
        let cwd = current
            .parent()
            .ok_or_else(|| io::Error::other("missing test directory"))?;
        // A file cannot contain another executable. This guarantees failure at
        // CreateProcessW without creating fixtures or risking an existing program.
        let missing = current.join("missing-parser.exe");
        for _ in 0..16 {
            match spawn(&missing, cwd) {
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::NotFound),
                Ok(mut child) => {
                    child.kill()?;
                    let _ = child.wait()?;
                    anyhow::bail!("nonexistent parser unexpectedly started");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn parser_does_not_inherit_unrelated_pipe_while_alive() -> anyhow::Result<()> {
        let host = crate::powershell::try_find_powershell_executable_blocking()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Windows PowerShell"))?;
        check_parser_pipe_isolation(host.as_path())
    }

    #[test]
    fn pwsh_parser_does_not_inherit_unrelated_pipe_while_alive() -> anyhow::Result<()> {
        let Some(host) = crate::powershell::try_find_pwsh_executable_blocking() else {
            return Ok(());
        };
        check_parser_pipe_isolation(host.as_path())
    }

    fn check_parser_pipe_isolation(host: &Path) -> anyhow::Result<()> {
        let (reader, writer) = pipe()?;
        let inherited_writer = inheritable(&writer)?;
        drop(writer);
        let mut parser = super::super::PowershellParserProcess::spawn(&host.to_string_lossy())?;
        // Prove the real parser is running, not a failed spawn that trivially releases handles.
        assert!(matches!(
            parser.parse("Get-Content file.txt")?,
            super::super::PowershellParseOutcome::Analysis(_)
        ));
        drop(inherited_writer);
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut reader = File::from(reader);
            let _ = tx.send(reader.read(&mut [0]));
        });
        // The pipe must close while the parser is deliberately kept alive, not
        // merely after its eventually scheduled shutdown has released leaked handles.
        let result = rx.recv_timeout(Duration::from_secs(2));
        let alive = parser
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("missing parser"))?
            .try_wait()?
            .is_none();
        drop(parser);
        // On regression, dropping the parser kills the owner that kept the pipe
        // alive. Do not join a possibly blocked reader on the failure path.
        assert!(
            alive,
            "parser must remain alive during the pipe isolation check"
        );
        assert_eq!(result??, 0);
        reader
            .join()
            .map_err(|_| io::Error::other("pipe reader panicked"))?;
        Ok(())
    }
}
