//! Windows named-pipe transport for the IDE context IPC client.

use std::cell::UnsafeCell;
use std::io;
use std::io::Read;
use std::io::Write;
use std::marker::PhantomPinned;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::ptr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::SyncSender;
use std::sync::mpsc::TryRecvError;
use std::sync::mpsc::TrySendError;
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use windows_sys::Win32::Foundation::BOOL;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_IO_INCOMPLETE;
use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::WAIT_FAILED;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TOKEN_USER;
use windows_sys::Win32::Security::TokenUser;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::IO::GetOverlappedResult;
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::OpenProcess;
use windows_sys::Win32::System::Threading::OpenProcessToken;
use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const TRUE: BOOL = 1;
const FALSE: BOOL = 0;
const NULL_HANDLE: HANDLE = 0;
const MAX_IN_FLIGHT_OPERATIONS: usize = 64;
const MAX_OPERATION_BYTES: usize = 64 * 1024;
const REAPER_POLL_INTERVAL: Duration = Duration::from_millis(10);

type PinnedOperation = Pin<Box<OverlappedOperation>>;

pub(super) struct WindowsPipeStream {
    handle: Arc<OwnedHandle>,
    deadline: Instant,
}

impl WindowsPipeStream {
    pub(super) fn connect(pipe_path: PathBuf, deadline: Instant) -> io::Result<Self> {
        let wide_path = pipe_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();

        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                NULL_HANDLE,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let handle = Arc::new(OwnedHandle(handle));
        validate_pipe_server_owner(handle.raw())?;

        Ok(Self { handle, deadline })
    }

    pub(super) fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }
}

impl Read for WindowsPipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let mut operation = OverlappedOperation::new_read(Arc::clone(&self.handle), buf.len())?;
        let bytes_to_read = operation.as_ref().buffer_len();
        let handle = operation.as_ref().handle_raw();
        let buffer = operation.as_mut().buffer_mut_ptr();
        let overlapped = operation.as_ref().overlapped_ptr();
        let result = unsafe {
            ReadFile(
                handle,
                buffer,
                bytes_to_read as u32,
                ptr::null_mut(),
                overlapped,
            )
        };

        let completed = OverlappedOperation::complete(operation, result, self.deadline)?;
        copy_completed_read(completed, buf)
    }
}

impl Write for WindowsPipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let operation = OverlappedOperation::new_write(Arc::clone(&self.handle), buf)?;
        let bytes_to_write = operation.as_ref().buffer_len();
        let handle = operation.as_ref().handle_raw();
        let buffer = operation.as_ref().buffer_ptr();
        let overlapped = operation.as_ref().overlapped_ptr();
        let result = unsafe {
            WriteFile(
                handle,
                buffer,
                bytes_to_write as u32,
                ptr::null_mut(),
                overlapped,
            )
        };

        OverlappedOperation::complete(operation, result, self.deadline)
            .map(|completed| completed.bytes_transferred)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct OverlappedOperation {
    overlapped: UnsafeCell<OVERLAPPED>,
    buffer: Vec<u8>,
    event: OwnedHandle,
    handle: Arc<OwnedHandle>,
    _permit: OperationPermit,
    _pin: PhantomPinned,
}

impl OverlappedOperation {
    fn new_read(handle: Arc<OwnedHandle>, requested_len: usize) -> io::Result<PinnedOperation> {
        let permit = OperationPermit::acquire()?;
        let buffer = vec![0_u8; requested_len.min(MAX_OPERATION_BYTES)];
        Self::new_with_permit(handle, buffer, permit)
    }

    fn new_write(handle: Arc<OwnedHandle>, input: &[u8]) -> io::Result<PinnedOperation> {
        let permit = OperationPermit::acquire()?;
        let buffer = input[..input.len().min(MAX_OPERATION_BYTES)].to_vec();
        Self::new_with_permit(handle, buffer, permit)
    }

    fn new_with_permit(
        handle: Arc<OwnedHandle>,
        buffer: Vec<u8>,
        permit: OperationPermit,
    ) -> io::Result<PinnedOperation> {
        let event = unsafe { CreateEventW(ptr::null(), TRUE, FALSE, ptr::null()) };
        if event == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut overlapped = unsafe { std::mem::zeroed::<OVERLAPPED>() };
        overlapped.hEvent = event;
        Ok(Box::pin(Self {
            overlapped: UnsafeCell::new(overlapped),
            buffer,
            event: OwnedHandle(event),
            handle,
            _permit: permit,
            _pin: PhantomPinned,
        }))
    }

    fn handle_raw(self: Pin<&Self>) -> HANDLE {
        self.get_ref().handle.raw()
    }

    fn event_raw(self: Pin<&Self>) -> HANDLE {
        self.get_ref().event.raw()
    }

    fn buffer_len(self: Pin<&Self>) -> usize {
        self.get_ref().buffer.len()
    }

    fn buffer_ptr(self: Pin<&Self>) -> *const u8 {
        self.get_ref().buffer.as_ptr()
    }

    fn buffer_mut_ptr(self: Pin<&mut Self>) -> *mut u8 {
        // SAFETY: the operation is pinned before its buffer pointer is exposed, and the buffer is
        // never resized while the Windows operation can reference it.
        unsafe { self.get_unchecked_mut().buffer.as_mut_ptr() }
    }

    fn overlapped_ptr(self: Pin<&Self>) -> *mut OVERLAPPED {
        // UnsafeCell permits Windows to update OVERLAPPED while Rust retains shared access to the
        // pinned owner. Callers keep the allocation fixed until completion is observed.
        self.get_ref().overlapped.get()
    }

    fn complete(
        operation: PinnedOperation,
        initial_result: BOOL,
        deadline: Instant,
    ) -> io::Result<CompletedOperation> {
        if initial_result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(error);
            }

            match unsafe {
                WaitForSingleObject(
                    operation.as_ref().event_raw(),
                    remaining_timeout_ms(deadline),
                )
            } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => {
                    return Self::cancel_and_reap(operation, timeout_io_error());
                }
                WAIT_FAILED => {
                    let error = io::Error::last_os_error();
                    return Self::cancel_and_reap(operation, error);
                }
                other => {
                    return Self::cancel_and_reap(
                        operation,
                        io::Error::other(format!("unexpected WaitForSingleObject result: {other}")),
                    );
                }
            }
        }

        let mut bytes_transferred = 0;
        let handle = operation.as_ref().handle_raw();
        let overlapped = operation.as_ref().overlapped_ptr();
        let result =
            unsafe { GetOverlappedResult(handle, overlapped, &mut bytes_transferred, FALSE) };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self::into_completed(operation, bytes_transferred as usize))
    }

    fn cancel_and_reap(
        operation: PinnedOperation,
        error: io::Error,
    ) -> io::Result<CompletedOperation> {
        let handle = operation.as_ref().handle_raw();
        let overlapped = operation.as_ref().overlapped_ptr();
        unsafe {
            CancelIoEx(handle, overlapped);
        }
        retire_pending_operation(operation);
        Err(error)
    }

    fn into_completed(operation: PinnedOperation, bytes_transferred: usize) -> CompletedOperation {
        // SAFETY: this is called only after the issuing API completed synchronously or the private
        // event was signaled and GetOverlappedResult reported successful completion. Windows no
        // longer references the pinned OVERLAPPED or buffer, so moving the allocation is safe.
        let operation = unsafe { Pin::into_inner_unchecked(operation) };
        let OverlappedOperation { buffer, .. } = *operation;
        CompletedOperation {
            bytes_transferred,
            buffer,
        }
    }
}

// SAFETY: ownership of an issued operation is transferred exactly once to the reaper. Its pinned
// allocation, buffer, event, and Arc-held pipe handle remain valid and are accessed by only one
// thread at a time until Windows reports completion.
unsafe impl Send for OverlappedOperation {}

struct CompletedOperation {
    bytes_transferred: usize,
    buffer: Vec<u8>,
}

fn copy_completed_read(
    completed: CompletedOperation,
    caller_buffer: &mut [u8],
) -> io::Result<usize> {
    if completed.bytes_transferred > completed.buffer.len()
        || completed.bytes_transferred > caller_buffer.len()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows reported more IDE context bytes than the read buffer can hold",
        ));
    }
    let bytes_transferred = completed.bytes_transferred;
    caller_buffer[..bytes_transferred].copy_from_slice(&completed.buffer[..bytes_transferred]);
    Ok(bytes_transferred)
}

struct OperationPermit {
    active: Arc<AtomicUsize>,
}

impl OperationPermit {
    fn acquire() -> io::Result<Self> {
        Self::acquire_from(active_operation_count(), MAX_IN_FLIGHT_OPERATIONS)
    }

    fn acquire_from(active: Arc<AtomicUsize>, limit: usize) -> io::Result<Self> {
        active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < limit).then_some(current + 1)
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "too many pending IDE context pipe operations",
                )
            })?;
        Ok(Self { active })
    }
}

impl Drop for OperationPermit {
    fn drop(&mut self) {
        let previous = self.active.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "operation permit count underflow");
    }
}

fn active_operation_count() -> Arc<AtomicUsize> {
    static ACTIVE: OnceLock<Arc<AtomicUsize>> = OnceLock::new();
    Arc::clone(ACTIVE.get_or_init(|| Arc::new(AtomicUsize::new(0))))
}

fn retire_pending_operation(operation: PinnedOperation) {
    let Some(sender) = pending_reaper_sender() else {
        // Keeping the permit in the forgotten operation bounds this safe fallback to the global
        // operation cap even if the reaper thread cannot be started.
        std::mem::forget(operation);
        return;
    };
    match sender.try_send(operation) {
        Ok(()) => {}
        Err(TrySendError::Full(operation) | TrySendError::Disconnected(operation)) => {
            // Full is prevented by the matching permit cap in production. Disconnection means the
            // reaper failed. In either case, retaining the allocation is safer than a kernel UAF.
            std::mem::forget(operation);
        }
    }
}

fn pending_reaper_sender() -> Option<&'static SyncSender<PinnedOperation>> {
    static REAPER: OnceLock<Option<SyncSender<PinnedOperation>>> = OnceLock::new();
    REAPER
        .get_or_init(|| {
            let (sender, receiver) = sync_channel(MAX_IN_FLIGHT_OPERATIONS);
            thread::Builder::new()
                .name("codex-ide-pipe-reaper".to_string())
                .spawn(move || run_pending_reaper(receiver))
                .ok()
                .map(|_| sender)
        })
        .as_ref()
}

fn run_pending_reaper(receiver: Receiver<PinnedOperation>) {
    let mut pending = Vec::with_capacity(MAX_IN_FLIGHT_OPERATIONS);
    loop {
        if pending.is_empty() {
            let Ok(operation) = receiver.recv() else {
                return;
            };
            pending.push(operation);
        }

        while pending.len() < MAX_IN_FLIGHT_OPERATIONS {
            match receiver.try_recv() {
                Ok(operation) => pending.push(operation),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    retain_pending_operations(pending);
                    return;
                }
            }
        }

        let mut index = 0;
        while index < pending.len() {
            if pending_operation_is_terminal(pending[index].as_ref()) {
                pending.swap_remove(index);
            } else {
                index += 1;
            }
        }

        if pending.len() == MAX_IN_FLIGHT_OPERATIONS {
            thread::sleep(REAPER_POLL_INTERVAL);
            continue;
        }
        match receiver.recv_timeout(REAPER_POLL_INTERVAL) {
            Ok(operation) => pending.push(operation),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                retain_pending_operations(pending);
                return;
            }
        }
    }
}

fn retain_pending_operations(pending: Vec<PinnedOperation>) {
    // A disconnected reaper has no owner that can keep polling. Retain each kernel-referenced
    // allocation (including its permit) rather than dropping it; the permit cap bounds this leak.
    for operation in pending {
        std::mem::forget(operation);
    }
}

fn pending_operation_is_terminal(operation: Pin<&OverlappedOperation>) -> bool {
    let mut bytes_transferred = 0;
    let handle = operation.as_ref().handle_raw();
    let overlapped = operation.as_ref().overlapped_ptr();
    let result = unsafe { GetOverlappedResult(handle, overlapped, &mut bytes_transferred, FALSE) };
    if result != 0 {
        return true;
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_IO_INCOMPLETE as i32) {
        return false;
    }

    // A completed operation that failed still signals its private event. Retain the allocation on
    // any ambiguous API error until Windows independently reports completion through that event.
    unsafe { WaitForSingleObject(operation.as_ref().event_raw(), 0) == WAIT_OBJECT_0 }
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

// SAFETY: Win32 HANDLE values may be used and closed from another thread. Arc ensures CloseHandle
// runs exactly once and never while an issued OverlappedOperation still holds the handle.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct TokenUserBuffer {
    buffer: Vec<u8>,
}

impl TokenUserBuffer {
    fn sid(&self) -> io::Result<windows_sys::Win32::Foundation::PSID> {
        if self.buffer.len() < std::mem::size_of::<TOKEN_USER>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "token user buffer is too small",
            ));
        }

        // GetTokenInformation writes TOKEN_USER into a byte buffer. Vec<u8> has
        // no TOKEN_USER alignment guarantee, so copy the fixed header out with
        // an unaligned read before using its SID pointer.
        let token_user =
            unsafe { std::ptr::read_unaligned(self.buffer.as_ptr() as *const TOKEN_USER) };
        Ok(token_user.User.Sid)
    }
}

fn validate_pipe_server_owner(pipe_handle: HANDLE) -> io::Result<()> {
    let mut server_process_id = 0;
    let result = unsafe { GetNamedPipeServerProcessId(pipe_handle, &mut server_process_id) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    let server_process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, server_process_id) };
    if server_process == 0 {
        return Err(io::Error::last_os_error());
    }
    let server_process = OwnedHandle(server_process);
    let server_token = open_process_token(server_process.raw())?;
    let current_token = open_process_token(unsafe { GetCurrentProcess() })?;
    let server_user = token_user(server_token.raw())?;
    let current_user = token_user(current_token.raw())?;

    if unsafe { EqualSid(server_user.sid()?, current_user.sid()?) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IDE context provider is not owned by the current user",
        ));
    }

    Ok(())
}

fn open_process_token(process: HANDLE) -> io::Result<OwnedHandle> {
    let mut token = 0;
    let result = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(OwnedHandle(token))
}

fn token_user(token: HANDLE) -> io::Result<TokenUserBuffer> {
    let mut return_length = 0;
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut return_length);
    }
    if return_length == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buffer = vec![0_u8; return_length as usize];
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr() as *mut _,
            return_length,
            &mut return_length,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(TokenUserBuffer { buffer })
}

fn remaining_timeout_ms(deadline: Instant) -> u32 {
    let now = Instant::now();
    if now >= deadline {
        return 0;
    }

    let millis = deadline.duration_since(now).as_millis().max(1);
    u32::try_from(millis).unwrap_or(u32::MAX)
}

fn timeout_io_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for IDE context")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_permits_cap_and_release_outstanding_work() {
        let active = Arc::new(AtomicUsize::new(0));
        let first = OperationPermit::acquire_from(Arc::clone(&active), 2).expect("first permit");
        let second = OperationPermit::acquire_from(Arc::clone(&active), 2).expect("second permit");
        let error = OperationPermit::acquire_from(Arc::clone(&active), 2)
            .err()
            .expect("third permit should be rejected");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(active.load(Ordering::Acquire), 2);

        drop(first);
        let replacement =
            OperationPermit::acquire_from(Arc::clone(&active), 2).expect("replacement permit");
        drop((second, replacement));
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn staging_operations_cap_oversized_buffers() {
        let handle = Arc::new(OwnedHandle(NULL_HANDLE));
        let oversized_len = MAX_OPERATION_BYTES + 17;

        let read = OverlappedOperation::new_read(Arc::clone(&handle), oversized_len)
            .expect("read operation");
        assert_eq!(read.as_ref().buffer_len(), MAX_OPERATION_BYTES);
        assert!(read.as_ref().get_ref().buffer.iter().all(|byte| *byte == 0));
        drop(read);

        let input = (0..oversized_len)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let write = OverlappedOperation::new_write(handle, &input).expect("write operation");
        assert_eq!(write.as_ref().buffer_len(), MAX_OPERATION_BYTES);
        assert_eq!(
            write.as_ref().get_ref().buffer,
            input[..MAX_OPERATION_BYTES]
        );
    }

    #[test]
    fn completed_read_copies_only_the_reported_prefix() {
        let mut caller = [b'x'; 6];
        let bytes = copy_completed_read(
            CompletedOperation {
                bytes_transferred: 3,
                buffer: b"abcdef".to_vec(),
            },
            &mut caller,
        )
        .expect("copy completed read");

        assert_eq!(bytes, 3);
        assert_eq!(&caller, b"abcxxx");
    }

    #[test]
    fn reaper_releases_owned_operation_only_after_terminal_signal() {
        use windows_sys::Win32::System::Threading::SetEvent;

        let active = Arc::new(AtomicUsize::new(0));
        let permit = OperationPermit::acquire_from(Arc::clone(&active), 1).expect("permit");
        let handle = Arc::new(OwnedHandle(NULL_HANDLE));
        let operation =
            OverlappedOperation::new_with_permit(Arc::clone(&handle), b"owned".to_vec(), permit)
                .expect("operation");
        let event = operation.as_ref().event_raw();
        assert_ne!(unsafe { SetEvent(event) }, 0);

        retire_pending_operation(operation);
        let deadline = Instant::now() + Duration::from_secs(1);
        while active.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(active.load(Ordering::Acquire), 0);
        assert_eq!(Arc::strong_count(&handle), 1);
    }
}
