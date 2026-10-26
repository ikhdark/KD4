#![allow(missing_docs)]
//! Watcher implementation for Windows' directory management APIs
//!
//! For more information see the [ReadDirectoryChangesW reference][ref].
//!
//! [ref]: https://msdn.microsoft.com/en-us/library/windows/desktop/aa363950(v=vs.85).aspx

use crate::paths::{WatchPath, absolute_path};
use crate::{BoundSender, Config, Receiver, Sender, bounded, unbounded};
use crate::{Error, EventHandler, RecursiveMode, Result, Watcher, WindowsPathSeparatorStyle};
use crate::{WatcherKind, event::*};
use std::alloc;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::raw::c_void;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED,
    ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY, FILE_NOTIFY_CHANGE_SIZE,
    FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx, OPEN_EXISTING,
    ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{CancelIo, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateSemaphoreW, INFINITE, ReleaseSemaphore, WaitForSingleObjectEx,
};

const BUF_SIZE: u32 = 16384;

#[derive(Clone, Copy)]
enum SeparatorStyle {
    Slash,
    Backslash,
}

impl SeparatorStyle {
    fn resolve(configured_style: WindowsPathSeparatorStyle, path: &Path) -> Self {
        match configured_style {
            WindowsPathSeparatorStyle::Auto => Self::from_path(path),
            WindowsPathSeparatorStyle::Slash => Self::Slash,
            WindowsPathSeparatorStyle::Backslash => Self::Backslash,
        }
    }

    fn from_path(path: &Path) -> Self {
        let mut has_forward_slash = false;
        let mut has_backslash = false;

        for ch in path.as_os_str().encode_wide() {
            if ch == '/' as u16 {
                has_forward_slash = true;
            } else if ch == '\\' as u16 {
                has_backslash = true;
            }

            if has_forward_slash && has_backslash {
                return Self::Backslash;
            }
        }

        if has_forward_slash {
            Self::Slash
        } else {
            Self::Backslash
        }
    }

    fn as_u16(self) -> u16 {
        match self {
            Self::Slash => '/' as u16,
            Self::Backslash => '\\' as u16,
        }
    }
}

fn trim_leading_separators(path: &[u16]) -> &[u16] {
    let mut start = 0;
    while start < path.len() && (path[start] == '/' as u16 || path[start] == '\\' as u16) {
        start += 1;
    }
    &path[start..]
}

fn windows_namespace_prefix_len(path: &[u16]) -> usize {
    let is_separator = |ch: u16| ch == '/' as u16 || ch == '\\' as u16;

    if path.len() >= 4
        && is_separator(path[0])
        && is_separator(path[1])
        && (path[2] == '?' as u16 || path[2] == '.' as u16)
        && is_separator(path[3])
    {
        4
    } else {
        0
    }
}

fn normalize_path_separators(path: PathBuf, separator_style: SeparatorStyle) -> PathBuf {
    let separator = separator_style.as_u16();
    let mut encoded_path: Vec<u16> = path.as_os_str().encode_wide().collect();
    let prefix_len = windows_namespace_prefix_len(&encoded_path);

    for ch in encoded_path.iter_mut().skip(prefix_len) {
        if *ch == '/' as u16 || *ch == '\\' as u16 {
            *ch = separator;
        }
    }

    PathBuf::from(OsString::from_wide(&encoded_path))
}

#[derive(Clone)]
struct ReadData {
    watch_path: PathBuf,   // key used in the server watch map
    dir: PathBuf,          // directory that is being watched
    reported_dir: PathBuf, // directory prefix used in emitted event paths
    file: Option<PathBuf>, // if a file is being watched, this is its reported path
    complete_sem: HANDLE,
    is_recursive: bool,
    separator_style: SeparatorStyle,
    stopping: Arc<AtomicBool>,
}

struct ReadDirectoryRequest {
    event_handler: Arc<Mutex<dyn EventHandler>>,
    event_kinds: EventKindMask,
    buffer: [u8; BUF_SIZE as usize],
    handle: HANDLE,
    data: ReadData,
    action_tx: Sender<Action>,
}

impl ReadDirectoryRequest {
    fn unwatch(&self) {
        let _ = self.action_tx.send(Action::Unwatch(
            self.data.watch_path.clone(),
            self.data.stopping.clone(),
        ));
    }
}

enum Action {
    Watch(WatchPath, RecursiveMode, SeparatorStyle),
    // Internal self-unwatch from the completion callback.
    Unwatch(PathBuf, Arc<AtomicBool>),
    // Public `Watcher::unwatch` path. This variant must ack only after `remove_watch` finishes so
    // the caller does not observe events after `unwatch()` returns.
    UnwatchAck(PathBuf),
    GetWatchedPaths(Sender<Vec<(PathBuf, RecursiveMode)>>),
    Stop,
    Configure(Config, BoundSender<Result<bool>>),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MetaEvent {
    SingleWatchComplete,
    WatcherAwakened,
}

struct WatchState {
    dir_handle: HANDLE,
    complete_sem: HANDLE,
    recursive_mode: RecursiveMode,
    reported_path: PathBuf,
    stopping: Arc<AtomicBool>,
}

struct ReadDirectoryChangesServer {
    tx: Sender<Action>,
    rx: Receiver<Action>,
    event_handler: Arc<Mutex<dyn EventHandler>>,
    event_kinds: EventKindMask,
    meta_tx: Sender<MetaEvent>,
    cmd_tx: Sender<Result<PathBuf>>,
    watches: HashMap<PathBuf, WatchState>,
    wakeup_sem: HANDLE,
}

impl ReadDirectoryChangesServer {
    fn start(
        event_handler: Arc<Mutex<dyn EventHandler>>,
        event_kinds: EventKindMask,
        meta_tx: Sender<MetaEvent>,
        cmd_tx: Sender<Result<PathBuf>>,
        wakeup_sem: HANDLE,
    ) -> Sender<Action> {
        let (action_tx, action_rx) = unbounded();
        // it is, in fact, ok to send the semaphore across threads
        let sem_temp = wakeup_sem as u64;
        let _ = thread::Builder::new()
            .name("notify-rs windows loop".to_string())
            .spawn({
                let tx = action_tx.clone();
                move || {
                    let wakeup_sem = sem_temp as HANDLE;
                    let server = ReadDirectoryChangesServer {
                        tx,
                        rx: action_rx,
                        event_handler,
                        event_kinds,
                        meta_tx,
                        cmd_tx,
                        watches: HashMap::new(),
                        wakeup_sem,
                    };
                    server.run();
                }
            });
        action_tx
    }

    fn run(mut self) {
        loop {
            // process all available actions first
            let mut stopped = false;

            while let Ok(action) = self.rx.try_recv() {
                match action {
                    Action::Watch(path, recursive_mode, separator_style) => {
                        let res =
                            self.add_watch(path, recursive_mode.is_recursive(), separator_style);
                        let _ = self.cmd_tx.send(res);
                    }
                    Action::Unwatch(path, stopping) => {
                        self.remove_watch_if_current(path, &stopping)
                    }
                    Action::UnwatchAck(path) => {
                        self.remove_watch(path.clone());
                        let _ = self.cmd_tx.send(Ok(path));
                    }
                    Action::GetWatchedPaths(tx) => {
                        let _ = tx.send(
                            self.watches
                                .iter()
                                .map(|(_path, state)| {
                                    (state.reported_path.clone(), state.recursive_mode)
                                })
                                .collect(),
                        );
                    }
                    Action::Stop => {
                        stopped = true;
                        for ws in self.watches.values() {
                            stop_watch(ws, &self.meta_tx);
                        }
                        break;
                    }
                    Action::Configure(config, tx) => {
                        self.configure_raw_mode(config, tx);
                    }
                }
            }

            if stopped {
                break;
            }

            unsafe {
                // wait with alertable flag so that the completion routine fires
                // External actions signal the semaphore. Internal unwatch
                // actions run in APC callbacks, which also return this wait.
                let waitres = WaitForSingleObjectEx(self.wakeup_sem, INFINITE, 1);
                if waitres == WAIT_OBJECT_0 {
                    let _ = self.meta_tx.send(MetaEvent::WatcherAwakened);
                }
            }
        }

        // we have to clean this up, since the watcher may be long gone
        unsafe {
            CloseHandle(self.wakeup_sem);
        }
    }

    fn add_watch(
        &mut self,
        path: WatchPath,
        is_recursive: bool,
        separator_style: SeparatorStyle,
    ) -> Result<PathBuf> {
        let metadata = path.absolute.metadata();
        // path must exist and be either a file or directory
        if metadata
            .as_ref()
            .map(|m| !m.is_dir() && !m.is_file())
            .unwrap_or(true)
        {
            return Err(
                Error::generic("Input watch path is neither a file nor a directory.")
                    .add_path(path.requested),
            );
        }

        let (watching_file, dir_target) = {
            if metadata.map(|m| m.is_dir()).unwrap_or(false) {
                (false, path.absolute.clone())
            } else {
                // emulate file watching by watching the parent directory
                (true, path.absolute.parent().unwrap().to_path_buf())
            }
        };
        let reported_dir = if watching_file {
            path.requested
                .parent()
                .map_or_else(PathBuf::new, Path::to_path_buf)
        } else {
            path.requested.clone()
        };

        let encoded_path: Vec<u16> = dir_target
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let handle;
        unsafe {
            handle = CreateFileW(
                encoded_path.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_DELETE | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            );

            if handle == INVALID_HANDLE_VALUE {
                return Err(if watching_file {
                    Error::generic(
                        "You attempted to watch a single file, but parent \
                         directory could not be opened.",
                    )
                    .add_path(path.requested)
                } else {
                    // TODO: Call GetLastError for better error info?
                    Error::path_not_found().add_path(path.requested)
                });
            }
        }
        let wf = if watching_file {
            Some(normalize_path_separators(
                path.requested.clone(),
                separator_style,
            ))
        } else {
            None
        };
        let watched_path = path.absolute.clone();
        // every watcher gets its own semaphore to signal completion
        let semaphore = unsafe { CreateSemaphoreW(ptr::null_mut(), 0, 1, ptr::null_mut()) };
        if semaphore.is_null() || semaphore == INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(handle);
            }
            return Err(
                Error::generic("Failed to create semaphore for watch.").add_path(path.requested)
            );
        }
        let stopping = Arc::new(AtomicBool::new(false));
        let rd = ReadData {
            watch_path: watched_path.clone(),
            dir: dir_target,
            reported_dir,
            file: wf,
            complete_sem: semaphore,
            is_recursive,
            separator_style,
            stopping: stopping.clone(),
        };
        let ws = WatchState {
            dir_handle: handle,
            complete_sem: semaphore,
            recursive_mode: if is_recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            },
            reported_path: path.requested,
            stopping,
        };
        if let Err(err) = start_read(
            &rd,
            self.event_handler.clone(),
            self.event_kinds,
            handle,
            self.tx.clone(),
        ) {
            unsafe {
                CloseHandle(handle);
                CloseHandle(semaphore);
            }
            return Err(err);
        }
        if let Some(ws) = self.watches.remove(&watched_path) {
            stop_watch(&ws, &self.meta_tx);
        }
        self.watches.insert(watched_path.clone(), ws);
        Ok(watched_path)
    }

    fn remove_watch(&mut self, path: PathBuf) {
        if let Some(ws) = self.watches.remove(&path) {
            stop_watch(&ws, &self.meta_tx);
        }
    }

    fn remove_watch_if_current(&mut self, path: PathBuf, stopping: &Arc<AtomicBool>) {
        if self
            .watches
            .get(&path)
            .is_some_and(|watch| Arc::ptr_eq(&watch.stopping, stopping))
        {
            self.remove_watch(path);
        }
    }

    fn configure_raw_mode(&mut self, _config: Config, tx: BoundSender<Result<bool>>) {
        tx.send(Ok(false))
            .expect("configuration channel disconnect");
    }
}

fn stop_watch(ws: &WatchState, meta_tx: &Sender<MetaEvent>) {
    // A successful completion may already be queued when cancellation starts. Mark the watch
    // first so that callback does not rearm ReadDirectoryChangesW with the handle closed below.
    ws.stopping.store(true, Ordering::Release);
    unsafe {
        let cio = CancelIo(ws.dir_handle);
        let ch = CloseHandle(ws.dir_handle);
        // have to wait for it, otherwise we leak the memory allocated for there read request
        if cio != 0 && ch != 0 {
            while WaitForSingleObjectEx(ws.complete_sem, INFINITE, 1) != WAIT_OBJECT_0 {
                // drain the apc queue, fix for https://github.com/notify-rs/notify/issues/287#issuecomment-801465550
            }
        }
        CloseHandle(ws.complete_sem);
    }
    let _ = meta_tx.send(MetaEvent::SingleWatchComplete);
}

fn start_read(
    rd: &ReadData,
    event_handler: Arc<Mutex<dyn EventHandler>>,
    event_kinds: EventKindMask,
    handle: HANDLE,
    action_tx: Sender<Action>,
) -> Result<()> {
    let request = Box::new(ReadDirectoryRequest {
        event_handler,
        event_kinds,
        handle,
        buffer: [0u8; BUF_SIZE as usize],
        data: rd.clone(),
        action_tx,
    });

    let flags = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION
        | FILE_NOTIFY_CHANGE_SECURITY;

    let monitor_subdir = if request.data.file.is_none() && request.data.is_recursive {
        1
    } else {
        0
    };

    unsafe {
        let overlapped = alloc::alloc_zeroed(alloc::Layout::new::<OVERLAPPED>()) as *mut OVERLAPPED;
        // When using callback based async requests, we are allowed to use the hEvent member
        // for our own purposes

        let request = Box::leak(request);
        (*overlapped).hEvent = request as *mut _ as _;

        // This is using an asynchronous call with a completion routine for receiving notifications
        // An I/O completion port would probably be more performant
        let ret = ReadDirectoryChangesW(
            handle,
            request.buffer.as_mut_ptr() as *mut c_void,
            BUF_SIZE,
            monitor_subdir,
            flags,
            &mut 0u32 as *mut u32, // not used for async reqs
            overlapped,
            Some(handle_event),
        );

        if ret == 0 {
            let err = std::io::Error::last_os_error();
            // error reading. retransmute request memory to allow drop.
            // Because of the error, ownership of the `overlapped` alloc was not passed
            // over to `ReadDirectoryChangesW`.
            // So we can claim ownership back.
            let _overlapped = Box::from_raw(overlapped);
            let request = Box::from_raw(request);
            let path = request
                .data
                .file
                .clone()
                .unwrap_or_else(|| request.data.reported_dir.clone());
            ReleaseSemaphore(request.data.complete_sem, 1, ptr::null_mut());
            return Err(Error::io(err).add_path(path));
        }
    }

    Ok(())
}

fn is_delete_pending(handle: HANDLE) -> bool {
    let mut info = FILE_STANDARD_INFO::default();
    let success = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of_val(&info) as u32,
        )
    };
    success != 0 && info.DeletePending
}

// Windows reports discarded notification details as either a zero-byte successful completion or
// ERROR_NOTIFY_ENUM_DIR.
fn completion_rescan_event(error_code: u32, bytes_written: u32) -> Option<Event> {
    (error_code == ERROR_NOTIFY_ENUM_DIR || (error_code == ERROR_SUCCESS && bytes_written == 0))
        .then(|| Event::new(EventKind::Other).set_flag(Flag::Rescan))
}

unsafe extern "system" fn handle_event(
    error_code: u32,
    bytes_written: u32,
    overlapped: *mut OVERLAPPED,
) {
    // SAFETY: Windows transfers these completion allocations exactly once;
    // start_read owns their construction and stopping fences handle retirement.
    unsafe {
        let overlapped: Box<OVERLAPPED> = Box::from_raw(overlapped);
        let request: Box<ReadDirectoryRequest> = Box::from_raw(overlapped.hEvent as *mut _);

        // CancelIo does not change a completion that was already queued successfully. Such a callback
        // can run while stop_watch is waiting for completion, after it has closed the directory handle.
        if request.data.stopping.load(Ordering::Acquire) {
            ReleaseSemaphore(request.data.complete_sem, 1, ptr::null_mut());
            return;
        }

        fn emit_event(event_handler: &Mutex<dyn EventHandler>, res: Result<Event>) {
            if let Ok(mut guard) = event_handler.lock() {
                let f: &mut dyn EventHandler = &mut *guard;
                f.handle_event(res);
            }
        }

        match error_code {
            ERROR_OPERATION_ABORTED => {
                // received when dir is unwatched or watcher is shutdown; return and let overlapped/request get drop-cleaned
                ReleaseSemaphore(request.data.complete_sem, 1, ptr::null_mut());
                return;
            }
            ERROR_ACCESS_DENIED => {
                // ReadDirectoryChangesW returns ERROR_ACCESS_DENIED both when the handle
                // has been invalidated (usually because the watched dir was deleted) and
                // when access has been revoked. The path can still be visible while the
                // open directory handle keeps it delete-pending, so check both states.
                // For directory watches, emit a Remove event so consumers learn the
                // watched path is gone, matching inotify IN_DELETE_SELF (#540) and
                // FSEvents ROOT_CHANGED+ITEM_REMOVED. File watches are excluded because
                // FILE_ACTION_REMOVED already fires for the file's parent.
                if matches!(request.data.dir.try_exists(), Ok(false))
                    || is_delete_pending(request.handle)
                {
                    if request.data.file.is_none() {
                        const KIND: EventKind = EventKind::Remove(RemoveKind::Folder);
                        if request.event_kinds.matches(&KIND) {
                            let path = normalize_path_separators(
                                request.data.reported_dir.clone(),
                                request.data.separator_style,
                            );
                            let event = Event::new(KIND).add_path(path);
                            emit_event(&request.event_handler, Ok(event));
                        }
                    }
                    request.unwatch();
                    ReleaseSemaphore(request.data.complete_sem, 1, ptr::null_mut());
                    return;
                }
            }
            ERROR_SUCCESS | ERROR_NOTIFY_ENUM_DIR => {
                // Continue below to rearm the watch before handling the completion.
            }
            _ => {
                // Some unidentified error occurred, log and unwatch the directory, then return.
                log::error!(
                    "unknown error in ReadDirectoryChangesW for directory {}: {}",
                    request.data.dir.display(),
                    error_code
                );
                request.unwatch();
                emit_event(
                    &request.event_handler,
                    Err(
                        Error::io(std::io::Error::from_raw_os_error(error_code as i32))
                            .add_path(request.data.watch_path.clone()),
                    ),
                );
                ReleaseSemaphore(request.data.complete_sem, 1, ptr::null_mut());
                return;
            }
        }

        // Get the next request queued up as soon as possible
        let rearm_error = start_read(
            &request.data,
            request.event_handler.clone(),
            request.event_kinds,
            request.handle,
            request.action_tx.clone(),
        )
        .err();

        if let Some(event) = completion_rescan_event(error_code, bytes_written) {
            emit_event(&request.event_handler, Ok(event));

            // Lost event details and a failed rearm are separate conditions. Report both so callers
            // know to rebuild their state and that this watch is no longer active.
            if let Some(err) = rearm_error {
                emit_event(&request.event_handler, Err(err));
                request.unwatch();
            }
            return;
        }

        // The FILE_NOTIFY_INFORMATION struct has a variable length due to the variable length
        // string as its last member. Each struct contains an offset for getting the next entry in
        // the buffer.
        let mut cur_offset: *const u8 = request.buffer.as_ptr();
        // In Wine, FILE_NOTIFY_INFORMATION structs are packed placed in the buffer;
        // they are aligned to 16bit (WCHAR) boundary instead of 32bit required by FILE_NOTIFY_INFORMATION.
        // Hence, we need to use `read_unaligned` here to avoid UB.
        let mut cur_entry = ptr::read_unaligned(cur_offset as *const FILE_NOTIFY_INFORMATION);
        loop {
            // filename length is size in bytes, so / 2
            let len = cur_entry.FileNameLength as usize / 2;
            let encoded_path: &[u16] = slice::from_raw_parts(
                cur_offset.offset(std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName) as isize)
                    as _,
                len,
            );
            // prepend root to get a full path
            let relative_path =
                PathBuf::from(OsString::from_wide(trim_leading_separators(encoded_path)));
            let absolute_path = request.data.dir.join(&relative_path);
            let path = normalize_path_separators(
                request.data.reported_dir.join(relative_path),
                request.data.separator_style,
            );

            // if we are watching a single file, ignore the event unless the path is exactly
            // the watched file
            let skip = match request.data.file {
                None => false,
                Some(ref watch_path) => *watch_path != path,
            };

            if !skip {
                log::trace!(
                    "Event: path = `{}`, action = {:?}",
                    path.display(),
                    cur_entry.Action
                );

                let newe = Event::new(EventKind::Any).add_path(path);

                // Filter events based on EventKindMask
                let event_kinds = request.event_kinds;
                let event_handler = |res: Result<Event>| {
                    match &res {
                        // Errors always pass through
                        Err(_) => emit_event(&request.event_handler, res),
                        // OK events only if they match the mask
                        Ok(e) if event_kinds.matches(&e.kind) => {
                            emit_event(&request.event_handler, res)
                        }
                        // Event filtered out
                        Ok(_) => {}
                    }
                };

                if cur_entry.Action == FILE_ACTION_RENAMED_OLD_NAME {
                    let mode = RenameMode::From;
                    let kind = ModifyKind::Name(mode);
                    let kind = EventKind::Modify(kind);
                    let ev = newe.set_kind(kind);
                    event_handler(Ok(ev))
                } else {
                    match cur_entry.Action {
                        FILE_ACTION_RENAMED_NEW_NAME => {
                            let kind = EventKind::Modify(ModifyKind::Name(RenameMode::To));
                            let ev = newe.set_kind(kind);
                            event_handler(Ok(ev));
                        }
                        FILE_ACTION_ADDED => {
                            let kind = std::fs::metadata(&absolute_path).map_or(
                                CreateKind::Any,
                                |metadata| {
                                    if metadata.is_dir() {
                                        CreateKind::Folder
                                    } else if metadata.is_file() {
                                        CreateKind::File
                                    } else {
                                        CreateKind::Any
                                    }
                                },
                            );
                            let kind = EventKind::Create(kind);
                            let ev = newe.set_kind(kind);
                            event_handler(Ok(ev));
                        }
                        FILE_ACTION_REMOVED => {
                            let kind = EventKind::Remove(RemoveKind::Any);
                            let ev = newe.set_kind(kind);
                            event_handler(Ok(ev));
                        }
                        FILE_ACTION_MODIFIED => {
                            let kind = EventKind::Modify(ModifyKind::Any);
                            let ev = newe.set_kind(kind);
                            event_handler(Ok(ev));
                        }
                        _ => (),
                    };
                }
            }

            if cur_entry.NextEntryOffset == 0 {
                break;
            }
            cur_offset = cur_offset.offset(cur_entry.NextEntryOffset as isize);
            cur_entry = ptr::read_unaligned(cur_offset as *const FILE_NOTIFY_INFORMATION);
        }

        if let Some(err) = rearm_error {
            emit_event(&request.event_handler, Err(err));
            request.unwatch();
        }
    }
}

/// Watcher implementation based on ReadDirectoryChanges
#[derive(Debug)]
pub struct ReadDirectoryChangesWatcher {
    tx: Sender<Action>,
    cmd_rx: Receiver<Result<PathBuf>>,
    wakeup_sem: HANDLE,
    windows_path_separator_style: WindowsPathSeparatorStyle,
}

impl ReadDirectoryChangesWatcher {
    pub fn create(
        event_handler: Arc<Mutex<dyn EventHandler>>,
        event_kinds: EventKindMask,
        meta_tx: Sender<MetaEvent>,
    ) -> Result<ReadDirectoryChangesWatcher> {
        Self::create_inner(
            event_handler,
            event_kinds,
            WindowsPathSeparatorStyle::Auto,
            meta_tx,
        )
    }

    fn create_inner(
        event_handler: Arc<Mutex<dyn EventHandler>>,
        event_kinds: EventKindMask,
        windows_path_separator_style: WindowsPathSeparatorStyle,
        meta_tx: Sender<MetaEvent>,
    ) -> Result<ReadDirectoryChangesWatcher> {
        let (cmd_tx, cmd_rx) = unbounded();

        let wakeup_sem = unsafe { CreateSemaphoreW(ptr::null_mut(), 0, 1, ptr::null_mut()) };
        if wakeup_sem.is_null() || wakeup_sem == INVALID_HANDLE_VALUE {
            return Err(Error::generic("Failed to create wakeup semaphore."));
        }

        let action_tx = ReadDirectoryChangesServer::start(
            event_handler,
            event_kinds,
            meta_tx,
            cmd_tx,
            wakeup_sem,
        );

        Ok(ReadDirectoryChangesWatcher {
            tx: action_tx,
            cmd_rx,
            wakeup_sem,
            windows_path_separator_style,
        })
    }

    fn wakeup_server(&self) {
        // Every external action must release the event-driven alertable wait.
        unsafe {
            ReleaseSemaphore(self.wakeup_sem, 1, ptr::null_mut());
        }
    }

    fn send_action_require_ack(&mut self, action: Action, pb: &PathBuf) -> Result<()> {
        self.tx
            .send(action)
            .map_err(|_| Error::generic("Error sending to internal channel"))?;

        // wake 'em up, we don't want to wait around for the ack
        self.wakeup_server();

        let ack_pb = self
            .cmd_rx
            .recv()
            .map_err(|_| Error::generic("Error receiving from command channel"))??;

        if pb.as_path() != ack_pb.as_path() {
            Err(Error::generic(&format!(
                "Expected ack for {:?} but got \
                 ack for {:?}",
                pb, ack_pb
            )))
        } else {
            Ok(())
        }
    }

    fn watch_inner(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        let separator_style = SeparatorStyle::resolve(self.windows_path_separator_style, path);
        let pb = WatchPath::new(path)?;
        self.send_action_require_ack(
            Action::Watch(pb.clone(), recursive_mode, separator_style),
            &pb.absolute,
        )
    }

    fn unwatch_inner(&mut self, path: &Path) -> Result<()> {
        let pb = absolute_path(path)?;
        self.send_action_require_ack(Action::UnwatchAck(pb.clone()), &pb)
    }

    fn watched_paths_inner(&self) -> Result<Vec<(PathBuf, RecursiveMode)>> {
        let (tx, rx) = unbounded();
        self.tx
            .send(Action::GetWatchedPaths(tx))
            .map_err(|_| Error::generic("Error sending to internal channel"))?;
        self.wakeup_server();
        rx.recv().map_err(Error::from)
    }
}

impl Watcher for ReadDirectoryChangesWatcher {
    fn new<F: EventHandler>(event_handler: F, config: Config) -> Result<Self> {
        // create dummy channel for meta event
        // TODO: determine the original purpose of this - can we remove it?
        let (meta_tx, _) = unbounded();
        let event_handler = Arc::new(Mutex::new(event_handler));
        Self::create_inner(
            event_handler,
            config.event_kinds(),
            config.windows_path_separator_style(),
            meta_tx,
        )
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.watch_inner(path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    fn configure(&mut self, config: Config) -> Result<bool> {
        let (tx, rx) = bounded(1);
        self.tx.send(Action::Configure(config, tx))?;
        self.wakeup_server();
        rx.recv()?
    }

    fn watched_paths(&self) -> Result<Vec<(PathBuf, RecursiveMode)>> {
        self.watched_paths_inner()
    }

    fn kind() -> crate::WatcherKind {
        WatcherKind::ReadDirectoryChangesWatcher
    }
}

impl Drop for ReadDirectoryChangesWatcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Action::Stop);
        // better wake it up
        self.wakeup_server();
    }
}

// `ReadDirectoryChangesWatcher` is not Send/Sync because of the semaphore Handle.
// As said elsewhere it's perfectly safe to send it across threads.
unsafe impl Send for ReadDirectoryChangesWatcher {}
// Because all public methods are `&mut self` it's also perfectly safe to share references.
unsafe impl Sync for ReadDirectoryChangesWatcher {}

