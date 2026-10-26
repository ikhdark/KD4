#![allow(clippy::expect_used)]
#![allow(clippy::upper_case_acronyms)]

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

use super::WinChild;
use crate::win::job::JobObject;
use crate::win::procthreadattr::ProcThreadAttributeList;
use anyhow::Error;
use anyhow::bail;
use anyhow::ensure;
use filedescriptor::FileDescriptor;
use filedescriptor::OwnedHandle;
use portable_pty::cmdbuilder::CommandBuilder;
use shared_library::shared_library;
use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Error as IoError;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use winapi::shared::minwindef::DWORD;
use winapi::shared::ntdef::NTSTATUS;
use winapi::shared::ntstatus::STATUS_SUCCESS;
use winapi::shared::winerror::HRESULT;
use winapi::shared::winerror::S_OK;
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::*;
use winapi::um::winbase::CREATE_UNICODE_ENVIRONMENT;
use winapi::um::winbase::EXTENDED_STARTUPINFO_PRESENT;
use winapi::um::winbase::STARTF_USESTDHANDLES;
use winapi::um::winbase::STARTUPINFOEXW;
use winapi::um::wincon::COORD;
use winapi::um::winnt::HANDLE;
use winapi::um::winnt::OSVERSIONINFOW;

pub type HPCON = HANDLE;

pub const PSEUDOCONSOLE_RESIZE_QUIRK: DWORD = 0x2;
// https://learn.microsoft.com/en-gb/windows/console/createpseudoconsole
// https://learn.microsoft.com/en-gb/windows/release-health/release-information
const MIN_CONPTY_BUILD: u32 = 17_763;

shared_library!(ConPtyFuncs,
    pub fn CreatePseudoConsole(
        size: COORD,
        hInput: HANDLE,
        hOutput: HANDLE,
        flags: DWORD,
        hpc: *mut HPCON
    ) -> HRESULT,
    pub fn ResizePseudoConsole(hpc: HPCON, size: COORD) -> HRESULT,
    pub fn ClosePseudoConsole(hpc: HPCON),
);

shared_library!(Ntdll,
    pub fn RtlGetVersion(
        version_info: *mut OSVERSIONINFOW
    ) -> NTSTATUS,
);

static CONPTY: LazyLock<Result<ConPtyFuncs, String>> = LazyLock::new(|| {
    ConPtyFuncs::open(Path::new("kernel32.dll")).map_err(|err| {
        format!("failed to load ConPTY; Windows 10 October 2018 or newer is required: {err:?}")
    })
});

pub fn conpty_supported() -> bool {
    windows_build_number().is_some_and(|build| build >= MIN_CONPTY_BUILD)
}

fn windows_build_number() -> Option<u32> {
    let ntdll = Ntdll::open(Path::new("ntdll.dll")).ok()?;
    // SAFETY: OSVERSIONINFOW contains integer and fixed-array fields for which the zero
    // representation is valid.
    let mut info: OSVERSIONINFOW = unsafe { mem::zeroed() };
    info.dwOSVersionInfoSize = mem::size_of::<OSVERSIONINFOW>() as u32;
    // SAFETY: info is writable OSVERSIONINFOW storage with its size initialized, and ntdll
    // keeps the function pointer loaded for this call.
    let status = unsafe { (ntdll.RtlGetVersion)(&mut info) };
    if status == STATUS_SUCCESS {
        Some(info.dwBuildNumber)
    } else {
        None
    }
}

pub struct PsuedoCon {
    con: HPCON,
    functions: &'static ConPtyFuncs,
    // Keep creation handles until a child is attached, then release our copies
    // so the external reader can observe a broken channel during teardown.
    creation_pipes: Mutex<Option<(FileDescriptor, FileDescriptor)>>,
}

// SAFETY: The pseudoconsole and its pipe handles are process-owned resources with no creator-
// thread affinity; moving this sole owner preserves their lifetime and exclusive cleanup.
unsafe impl Send for PsuedoCon {}
// SAFETY: Shared methods only read the owned handle and issue Win32 resize or process-creation
// requests. Pipe ownership is protected by a mutex, and exclusive Drop prevents closing the
// pseudoconsole while borrowed.
unsafe impl Sync for PsuedoCon {}

impl Drop for PsuedoCon {
    fn drop(&mut self) {
        // SAFETY: self owns the successfully created pseudoconsole. Callers must keep
        // draining its output (or close their reader) until teardown completes.
        unsafe { (self.functions.ClosePseudoConsole)(self.con) };
    }
}

impl PsuedoCon {
    pub fn raw_handle(&self) -> HPCON {
        self.con
    }

    pub fn new(size: COORD, input: FileDescriptor, output: FileDescriptor) -> Result<Self, Error> {
        let functions = CONPTY.as_ref().map_err(|err| anyhow::anyhow!("{err}"))?;
        let mut con: HPCON = INVALID_HANDLE_VALUE;
        // SAFETY: input and output own live pipe handles; con is writable HPCON storage, and
        // both pipes are retained in Self on success.
        let result = unsafe {
            (functions.CreatePseudoConsole)(
                size,
                input.as_raw_handle() as _,
                output.as_raw_handle() as _,
                PSEUDOCONSOLE_RESIZE_QUIRK,
                &mut con,
            )
        };
        ensure!(
            result == S_OK,
            "failed to create psuedo console: HRESULT {result}"
        );
        Ok(Self {
            con,
            functions,
            creation_pipes: Mutex::new(Some((input, output))),
        })
    }

    pub fn resize(&self, size: COORD) -> Result<(), Error> {
        // SAFETY: self owns the live pseudoconsole for this call; dimensions are scalar values
        // validated by Windows.
        let result = unsafe { (self.functions.ResizePseudoConsole)(self.con, size) };
        ensure!(
            result == S_OK,
            "failed to resize console to {}x{}: HRESULT: {}",
            size.X,
            size.Y,
            result
        );
        Ok(())
    }

    pub fn spawn_command(&self, cmd: CommandBuilder) -> anyhow::Result<WinChild> {
        let job = Arc::new(JobObject::create()?);
        job.require_descendant_containment()?;
        // SAFETY: STARTUPINFOEXW is an integer-and-pointer Windows structure with a valid all-
        // zero initial representation.
        let mut si: STARTUPINFOEXW = unsafe { mem::zeroed() };
        si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;

        let mut attrs = ProcThreadAttributeList::with_capacity(/*num_attributes*/ 2)?;
        attrs.set_pty(self.con)?;
        attrs.set_job(job.as_raw_handle().cast())?;
        si.lpAttributeList = attrs.as_mut_ptr();

        // SAFETY: PROCESS_INFORMATION contains integer and handle fields for which the zero
        // initial representation is valid.
        let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

        let (mut exe, mut cmdline) = build_cmdline(&cmd)?;
        let cmd_os = OsString::from_wide(&cmdline);

        let cwd = resolve_current_directory(&cmd)?;
        let mut env_block = build_environment_block(&cmd)?;

        // SAFETY: exe, cmdline, cwd, and env_block are terminated buffers retained for the
        // call; si and pi are initialized writable structures and attrs retains the console/job
        // attributes.
        let res = unsafe {
            CreateProcessW(
                exe.as_mut_ptr(),
                cmdline.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                env_block.as_mut_ptr() as *mut _,
                cwd.as_ref().map_or(ptr::null(), std::vec::Vec::as_ptr),
                &mut si.StartupInfo,
                &mut pi,
            )
        };
        if res == 0 {
            let err = IoError::last_os_error();
            let msg = format!(
                "CreateProcessW `{:?}` in cwd `{:?}` failed: {}",
                cmd_os,
                cwd.as_ref().map(|c| OsString::from_wide(c)),
                err
            );
            log::error!("{msg}");
            bail!("{msg}");
        }

        crate::process_observation::observe_process_start(
            "unified_conpty",
            crate::process_observation::ProcessRole::RequestedRoot,
            Some(pi.dwProcessId),
            None,
        );
        // SAFETY: Successful CreateProcessW returned this new thread handle, whose sole
        // ownership is transferred to OwnedHandle.
        let _main_thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread as _) };
        // SAFETY: Successful CreateProcessW returned this new process handle, whose sole
        // ownership is transferred to OwnedHandle.
        let proc = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as _) };

        // CreateProcessW has attached the child; ConPTY now owns its pipe copies.
        self.creation_pipes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Ok(WinChild::new(proc, job))
    }
}

fn resolve_current_directory(cmd: &CommandBuilder) -> anyhow::Result<Option<Vec<u16>>> {
    let dir = cmd.get_cwd().cloned().or_else(|| {
        cmd.get_env("USERPROFILE")
            .filter(|path| Path::new(path).is_dir())
            .map(OsStr::to_os_string)
    });
    let Some(dir) = dir else {
        return Ok(None);
    };
    let dir = if Path::new(&dir).is_relative() {
        env::current_dir()?.join(&dir)
    } else {
        dir.into()
    };
    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    ensure!(
        !wide.contains(&0),
        "current directory contains a null character"
    );
    wide.push(0);
    Ok(Some(wide))
}

fn build_environment_block(cmd: &CommandBuilder) -> anyhow::Result<Vec<u16>> {
    let mut block = Vec::new();
    for (key, value) in cmd.iter_full_env_as_str() {
        ensure!(
            !key.contains('\0') && !value.contains('\0'),
            "environment contains a null character"
        );
        block.extend(OsStr::new(key).encode_wide());
        block.push(b'=' as u16);
        block.extend(OsStr::new(value).encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn build_cmdline(cmd: &CommandBuilder) -> anyhow::Result<(Vec<u16>, Vec<u16>)> {
    let requested: OsString = if cmd.is_default_prog() {
        cmd.get_env("ComSpec")
            .unwrap_or(OsStr::new("cmd.exe"))
            .to_os_string()
    } else {
        let argv = cmd.get_argv();
        let Some(first) = argv.first() else {
            anyhow::bail!("missing program name");
        };
        first.clone()
    };

    ensure!(
        !requested.encode_wide().any(|c| c == 0),
        "program name contains a null character"
    );
    let exe_os = search_path(cmd, &requested)?;
    // Batch argv needs CMD-specific escaping, not the native argv encoder below.
    // Refuse before dispatch instead of launching an invalid image or interpolating
    // caller arguments into an unsafe shell script.
    ensure!(
        !Path::new(&exe_os).extension().is_some_and(|ext| {
            ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat")
        }),
        "ConPTY cannot execute batch argv directly; invoke cmd.exe /d /c with an explicit shell script"
    );
    let argv = cmd.get_argv();
    let args = argv.get(1..).unwrap_or_default();
    let cmd_payload_index = crate::windows_cmd_payload_index(&exe_os, args);
    let mut cmdline = Vec::new();
    append_quoted(&exe_os, &mut cmdline);
    for (index, arg) in args.iter().enumerate() {
        cmdline.push(' ' as u16);
        ensure!(
            !arg.encode_wide().any(|c| c == 0),
            "invalid encoding for command line argument {arg:?}"
        );
        if Some(index) == cmd_payload_index {
            cmdline.push('"' as u16);
            cmdline.extend(arg.encode_wide());
            cmdline.push('"' as u16);
        } else {
            append_quoted(arg, &mut cmdline);
        }
    }
    cmdline.push(0);

    let mut exe: Vec<u16> = exe_os.encode_wide().collect();
    exe.push(0);

    Ok((exe, cmdline))
}

fn search_path(cmd: &CommandBuilder, exe: &OsStr) -> anyhow::Result<OsString> {
    let cwd = match resolve_current_directory(cmd)? {
        Some(wide) => std::path::PathBuf::from(OsString::from_wide(&wide[..wide.len() - 1])),
        None => env::current_dir()?,
    };
    let host_extensions = env::var_os("PATHEXT");
    let extensions = cmd
        .get_env("PATHEXT")
        .or(host_extensions.as_deref())
        .unwrap_or(OsStr::new(".EXE"));
    let extensions = env::split_paths(extensions)
        .filter_map(|ext| {
            ext.to_str()
                .map(|ext| ext.trim_start_matches('.').to_owned())
        })
        .filter(|ext| !ext.is_empty())
        .collect::<Vec<_>>();
    let probe = |path: std::path::PathBuf| {
        if path.is_file() {
            return Some(path.into_os_string());
        }
        if path.extension().is_none() {
            for ext in &extensions {
                let candidate = path.with_extension(ext);
                if candidate.is_file() {
                    return Some(candidate.into_os_string());
                }
            }
        }
        None
    };
    let qualified = Path::new(exe).components().count() != 1
        || Path::new(exe).file_name().is_none()
        || exe
            .encode_wide()
            .any(|ch| ch == u16::from(b'/') || ch == u16::from(b'\\'));
    if qualified {
        // GetFullPathName semantics resolve drive-relative and root-relative
        // spellings without changing process-global cwd.
        let candidate = std::path::absolute(cwd.join(exe))?;
        return probe(candidate).ok_or_else(|| {
            anyhow::anyhow!("program {exe:?} not found relative to execution directory {cwd:?}")
        });
    }
    let host_path = env::var_os("PATH");
    if let Some(path) = cmd.get_env("PATH").or(host_path.as_deref()) {
        for path in env::split_paths(path) {
            if let Some(candidate) = probe(std::path::absolute(cwd.join(path).join(exe))?) {
                return Ok(candidate);
            }
        }
    }
    // The execution directory is only a last resort for a bare name: a file
    // planted in the workspace must not shadow the same program on PATH.
    if let Some(candidate) = probe(cwd.join(exe)) {
        return Ok(candidate);
    }

    bail!("program {exe:?} not found in the execution directory or PATH")
}

fn append_quoted(arg: &OsStr, cmdline: &mut Vec<u16>) {
    if !arg.is_empty()
        && !arg.encode_wide().any(|c| {
            c == ' ' as u16
                || c == '\t' as u16
                || c == '\n' as u16
                || c == '\x0b' as u16
                || c == '\"' as u16
        })
    {
        cmdline.extend(arg.encode_wide());
        return;
    }
    cmdline.push('"' as u16);

    let arg: Vec<_> = arg.encode_wide().collect();
    let mut i = 0;
    while i < arg.len() {
        let mut num_backslashes = 0;
        while i < arg.len() && arg[i] == '\\' as u16 {
            i += 1;
            num_backslashes += 1;
        }

        if i == arg.len() {
            for _ in 0..num_backslashes * 2 {
                cmdline.push('\\' as u16);
            }
            break;
        } else if arg[i] == b'"' as u16 {
            for _ in 0..num_backslashes * 2 + 1 {
                cmdline.push('\\' as u16);
            }
            cmdline.push(arg[i]);
        } else {
            for _ in 0..num_backslashes {
                cmdline.push('\\' as u16);
            }
            cmdline.push(arg[i]);
        }
        i += 1;
    }
    cmdline.push('"' as u16);
}

