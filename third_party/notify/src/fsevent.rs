//! Watcher implementation for Darwin's FSEvents API
//!
//! The FSEvents API provides a mechanism to notify clients about directories they ought to re-scan
//! in order to keep their internal data structures up-to-date with respect to the true state of
//! the file system. (For example, when files or directories are created, modified, or removed.) It
//! sends these notifications "in bulk", possibly notifying the client of changes to several
//! directories in a single callback.
//!
//! For more information see the [FSEvents API reference][ref].
//!
//! TODO: document event translation
//!
//! [ref]: https://developer.apple.com/library/mac/documentation/Darwin/Reference/FSEvents_Ref/

#![allow(non_upper_case_globals, dead_code)]

use crate::paths::{absolute_path, reported_path};
use crate::{
    Config, Error, ErrorKind, EventHandler, EventKindMask, RecursiveMode, Result, Sender, Watcher,
    unbounded,
};
use crate::{PathOp, event::*};
use objc2_core_foundation as cf;
use objc2_core_services as fs;
use std::collections::HashMap;
use std::ffi::{CStr, OsStr};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

bitflags::bitflags! {
  #[repr(C)]
  #[derive(Debug)]
  struct StreamFlags: u32 {
    const NONE = fs::kFSEventStreamEventFlagNone;
    const MUST_SCAN_SUBDIRS = fs::kFSEventStreamEventFlagMustScanSubDirs;
    const USER_DROPPED = fs::kFSEventStreamEventFlagUserDropped;
    const KERNEL_DROPPED = fs::kFSEventStreamEventFlagKernelDropped;
    const IDS_WRAPPED = fs::kFSEventStreamEventFlagEventIdsWrapped;
    const HISTORY_DONE = fs::kFSEventStreamEventFlagHistoryDone;
    const ROOT_CHANGED = fs::kFSEventStreamEventFlagRootChanged;
    const MOUNT = fs::kFSEventStreamEventFlagMount;
    const UNMOUNT = fs::kFSEventStreamEventFlagUnmount;
    const ITEM_CREATED = fs::kFSEventStreamEventFlagItemCreated;
    const ITEM_REMOVED = fs::kFSEventStreamEventFlagItemRemoved;
    const INODE_META_MOD = fs::kFSEventStreamEventFlagItemInodeMetaMod;
    const ITEM_RENAMED = fs::kFSEventStreamEventFlagItemRenamed;
    const ITEM_MODIFIED = fs::kFSEventStreamEventFlagItemModified;
    const FINDER_INFO_MOD = fs::kFSEventStreamEventFlagItemFinderInfoMod;
    const ITEM_CHANGE_OWNER = fs::kFSEventStreamEventFlagItemChangeOwner;
    const ITEM_XATTR_MOD = fs::kFSEventStreamEventFlagItemXattrMod;
    const IS_FILE = fs::kFSEventStreamEventFlagItemIsFile;
    const IS_DIR = fs::kFSEventStreamEventFlagItemIsDir;
    const IS_SYMLINK = fs::kFSEventStreamEventFlagItemIsSymlink;
    const OWN_EVENT = fs::kFSEventStreamEventFlagOwnEvent;
    const IS_HARDLINK = fs::kFSEventStreamEventFlagItemIsHardlink;
    const IS_LAST_HARDLINK = fs::kFSEventStreamEventFlagItemIsLastHardlink;
    const ITEM_CLONED = fs::kFSEventStreamEventFlagItemCloned;
  }
}

/// FSEvents-based `Watcher` implementation
pub struct FsEventWatcher {
    since_when: fs::FSEventStreamEventId,
    latency: cf::CFTimeInterval,
    flags: fs::FSEventStreamCreateFlags,
    event_handler: Arc<Mutex<dyn EventHandler>>,
    runloop: Option<RunLoopHandle>,
    watches: HashMap<PathBuf, WatchEntry>,
    event_kinds: EventKindMask,
}

// `cf_path` is kept out of `WatchInfo` because `WatchInfo` is cloned into the stream
// context, which must stay `Send + Sync`.
#[derive(Debug)]
struct WatchEntry {
    info: WatchInfo,
    cf_path: cf::CFRetained<cf::CFString>,
    device: u64,
}

#[derive(Clone, Debug)]
struct WatchInfo {
    is_recursive: bool,
    reported_path: PathBuf,
}

// FSEvents applies the path limit across live streams, so all watcher instances
// in this process must share the same count.
static ACTIVE_FSEVENTS_PATHS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct FseventsPathReservation {
    active_paths: &'static AtomicUsize,
    path_count: usize,
}

impl FseventsPathReservation {
    fn acquire(
        active_paths: &'static AtomicUsize,
        path_count: usize,
        budget: usize,
    ) -> std::result::Result<Self, usize> {
        active_paths
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active_path_count| {
                active_path_count
                    .checked_add(path_count)
                    .filter(|&combined_path_count| combined_path_count <= budget)
            })
            .map(|_| Self {
                active_paths,
                path_count,
            })
    }
}

impl Drop for FseventsPathReservation {
    fn drop(&mut self) {
        let previous = self
            .active_paths
            .fetch_sub(self.path_count, Ordering::Relaxed);
        debug_assert!(previous >= self.path_count);
    }
}

#[derive(Debug)]
struct RunLoopHandle {
    runloop: cf::CFRetained<cf::CFRunLoop>,
    stop_flag: Arc<AtomicBool>,
    thread_handle: thread::JoinHandle<()>,
}

impl fmt::Debug for FsEventWatcher {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("FsEventWatcher")
            .field("since_when", &self.since_when)
            .field("latency", &self.latency)
            .field("flags", &self.flags)
            .field("event_handler", &Arc::as_ptr(&self.event_handler))
            .field("runloop", &self.runloop)
            .field("watches", &self.watches)
            .finish()
    }
}

// FsEventWatcher is not Send/Sync automatically.
// It's Send because the pointer is not used in other threads.
unsafe impl Send for FsEventWatcher {}

// It's Sync because all methods that change the mutable state use `&mut self`.
unsafe impl Sync for FsEventWatcher {}

fn translate_flags(flags: StreamFlags, precise: bool) -> Vec<Event> {
    let mut evs = Vec::new();
    translate_flags_with(flags, precise, |ev| evs.push(ev));
    evs
}

// Keep this in sync with `translate_flags_with`; the callback uses it to avoid path clones.
fn translated_event_count(flags: &StreamFlags, precise: bool) -> usize {
    if flags.contains(StreamFlags::HISTORY_DONE) {
        return 0;
    }

    let mut count = usize::from(flags.contains(StreamFlags::MUST_SCAN_SUBDIRS));
    if !precise {
        return count + 1;
    }

    let root_changed = flags.contains(StreamFlags::ROOT_CHANGED);
    count += usize::from(root_changed);
    count += usize::from(flags.contains(StreamFlags::MOUNT));
    count += usize::from(flags.contains(StreamFlags::UNMOUNT));
    count += usize::from(flags.contains(StreamFlags::ITEM_CREATED));
    count += usize::from(flags.contains(StreamFlags::ITEM_RENAMED) && !root_changed);
    count += usize::from(flags.contains(StreamFlags::INODE_META_MOD));
    count += usize::from(flags.contains(StreamFlags::FINDER_INFO_MOD));
    count += usize::from(flags.contains(StreamFlags::ITEM_CHANGE_OWNER));
    count += usize::from(flags.contains(StreamFlags::ITEM_XATTR_MOD));
    count += usize::from(flags.contains(StreamFlags::ITEM_MODIFIED));
    count += usize::from(flags.contains(StreamFlags::ITEM_REMOVED) && !root_changed);
    count
}

fn translate_flags_with(flags: StreamFlags, precise: bool, mut emit: impl FnMut(Event)) {
    // «Denotes a sentinel event sent to mark the end of the "historical" events
    // sent as a result of specifying a `sinceWhen` value in the FSEvents.Create
    // call that created this event stream. After invoking the client's callback
    // with all the "historical" events that occurred before now, the client's
    // callback will be invoked with an event where the HistoryDone flag is set.
    // The client should ignore the path supplied in this callback.»
    // — https://www.mbsplugins.eu/FSEventsNextEvent.shtml
    //
    // As a result, we just stop processing here and return an empty vec, which
    // will ignore this completely and not emit any Events whatsoever.
    if flags.contains(StreamFlags::HISTORY_DONE) {
        return;
    }

    // `ITEM_CLONED` can be present alongside other flags (including create/modify/remove).
    // Preserve any existing `info` (like "root changed"), but annotate otherwise so downstream
    // can detect and filter clone-related events. See https://github.com/notify-rs/notify/issues/465.
    let clone_related = precise && flags.contains(StreamFlags::ITEM_CLONED);
    let own_process_id = if precise && flags.contains(StreamFlags::OWN_EVENT) {
        Some(std::process::id())
    } else {
        None
    };

    let mut emit_event = |mut ev: Event| {
        if clone_related && ev.info().is_none() {
            ev.attrs.set_info("is: clone");
        }
        if let Some(process_id) = own_process_id {
            ev.attrs.set_process_id(process_id);
        }
        emit(ev);
    };

    // FSEvents provides two possible hints as to why events were dropped,
    // however documentation on what those mean is scant, so we just pass them
    // through in the info attr field. The intent is clear enough, and the
    // additional information is provided if the user wants it.
    if flags.contains(StreamFlags::MUST_SCAN_SUBDIRS) {
        let e = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        emit_event(if flags.contains(StreamFlags::USER_DROPPED) {
            e.set_info("rescan: user dropped")
        } else if flags.contains(StreamFlags::KERNEL_DROPPED) {
            e.set_info("rescan: kernel dropped")
        } else {
            e
        });
    }

    // In imprecise mode, let's not even bother parsing the kind of the event
    // except for the above very special events.
    if !precise {
        emit(Event::new(EventKind::Any));
        return;
    }

    // A watched root changed (renamed or removed). If the flags provide a hint,
    // prefer that over guessing. Otherwise, treat it as a removal to avoid
    // misclassifying a delete as a rename.
    let root_changed = flags.contains(StreamFlags::ROOT_CHANGED);
    if root_changed {
        let kind = if flags.contains(StreamFlags::ITEM_REMOVED) {
            if flags.contains(StreamFlags::IS_DIR) {
                EventKind::Remove(RemoveKind::Folder)
            } else if flags.contains(StreamFlags::IS_FILE) {
                EventKind::Remove(RemoveKind::File)
            } else {
                EventKind::Remove(RemoveKind::Any)
            }
        } else if flags.contains(StreamFlags::ITEM_RENAMED) {
            EventKind::Modify(ModifyKind::Name(RenameMode::From))
        } else {
            EventKind::Remove(RemoveKind::Any)
        };

        emit_event(Event::new(kind).set_info("root changed"));
    }

    // A path was mounted at the event path; we treat that as a create.
    if flags.contains(StreamFlags::MOUNT) {
        emit_event(Event::new(EventKind::Create(CreateKind::Other)).set_info("mount"));
    }

    // A path was unmounted at the event path; we treat that as a remove.
    if flags.contains(StreamFlags::UNMOUNT) {
        emit_event(Event::new(EventKind::Remove(RemoveKind::Other)).set_info("mount"));
    }

    if flags.contains(StreamFlags::ITEM_CREATED) {
        emit_event(if flags.contains(StreamFlags::IS_DIR) {
            Event::new(EventKind::Create(CreateKind::Folder))
        } else if flags.contains(StreamFlags::IS_FILE) {
            Event::new(EventKind::Create(CreateKind::File))
        } else {
            let e = Event::new(EventKind::Create(CreateKind::Other));
            if flags.contains(StreamFlags::IS_SYMLINK) {
                e.set_info("is: symlink")
            } else if flags.contains(StreamFlags::IS_HARDLINK) {
                e.set_info("is: hardlink")
            } else if flags.contains(StreamFlags::ITEM_CLONED) {
                e.set_info("is: clone")
            } else {
                Event::new(EventKind::Create(CreateKind::Any))
            }
        });
    }

    // FSEvents provides no mechanism to associate the old and new sides of a
    // rename event.
    // Avoid emitting duplicate events around a root change by checking `root_changed`.
    if flags.contains(StreamFlags::ITEM_RENAMED) && !root_changed {
        emit_event(Event::new(EventKind::Modify(ModifyKind::Name(
            RenameMode::Any,
        ))));
    }

    // This is only described as "metadata changed", but it may be that it's
    // only emitted for some more precise subset of events... if so, will need
    // amending, but for now we have an Any-shaped bucket to put it in.
    if flags.contains(StreamFlags::INODE_META_MOD) {
        emit_event(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any,
        ))));
    }

    if flags.contains(StreamFlags::FINDER_INFO_MOD) {
        emit_event(
            Event::new(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Other)))
                .set_info("meta: finder info"),
        );
    }

    if flags.contains(StreamFlags::ITEM_CHANGE_OWNER) {
        emit_event(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Ownership,
        ))));
    }

    if flags.contains(StreamFlags::ITEM_XATTR_MOD) {
        emit_event(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Extended,
        ))));
    }

    // This is specifically described as a data change, which we take to mean
    // is a content change.
    if flags.contains(StreamFlags::ITEM_MODIFIED) {
        emit_event(Event::new(EventKind::Modify(ModifyKind::Data(
            DataChange::Content,
        ))));
    }

    // Avoid emitting duplicate events around a root change by checking `root_changed`.
    if flags.contains(StreamFlags::ITEM_REMOVED) && !root_changed {
        emit_event(if flags.contains(StreamFlags::IS_DIR) {
            Event::new(EventKind::Remove(RemoveKind::Folder))
        } else if flags.contains(StreamFlags::IS_FILE) {
            Event::new(EventKind::Remove(RemoveKind::File))
        } else {
            let e = Event::new(EventKind::Remove(RemoveKind::Other));
            if flags.contains(StreamFlags::IS_SYMLINK) {
                e.set_info("is: symlink")
            } else if flags.contains(StreamFlags::IS_HARDLINK) {
                e.set_info("is: hardlink")
            } else if flags.contains(StreamFlags::ITEM_CLONED) {
                e.set_info("is: clone")
            } else {
                Event::new(EventKind::Remove(RemoveKind::Any))
            }
        });
    }
}

struct StreamContextInfo {
    event_handler: Arc<Mutex<dyn EventHandler>>,
    recursive_info: HashMap<PathBuf, WatchInfo>,
    event_kinds: EventKindMask,
}

// Free the context when the stream created by `FSEventStreamCreate` is released.
unsafe extern "C-unwind" fn release_context(info: *const libc::c_void) {
    // Safety:
    // - The [documentation] for `FSEventStreamContext` states that `release` is only
    //   called when the stream is deallocated, so it is safe to convert `info` back into a
    //   box and drop it.
    //
    // [docs]: https://developer.apple.com/documentation/coreservices/fseventstreamcontext?language=objc
    unsafe {
        drop(Box::from_raw(
            info as *const StreamContextInfo as *mut StreamContextInfo,
        ));
    }
}

impl FsEventWatcher {
    fn from_event_handler(
        event_handler: Arc<Mutex<dyn EventHandler>>,
        event_kinds: EventKindMask,
        latency: cf::CFTimeInterval,
    ) -> Result<Self> {
        Ok(FsEventWatcher {
            since_when: fs::kFSEventStreamEventIdSinceNow,
            latency,
            flags: fs::kFSEventStreamCreateFlagFileEvents
                | fs::kFSEventStreamCreateFlagNoDefer
                | fs::kFSEventStreamCreateFlagWatchRoot,
            event_handler,
            runloop: None,
            watches: HashMap::new(),
            event_kinds,
        })
    }

    fn watch_inner(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.stop();
        let result = self.append_path(path, recursive_mode);
        self.run()?;
        result
    }

    fn unwatch_inner(&mut self, path: &Path) -> Result<()> {
        self.stop();
        let result = self.remove_path(path);
        self.run()?;
        result
    }

    fn update_paths_inner(
        &mut self,
        ops: Vec<crate::PathOp>,
    ) -> crate::StdResult<(), crate::UpdatePathsError> {
        self.stop();

        let result = crate::update_paths(ops, |op| match op {
            crate::PathOp::Watch(path, config) => self
                .append_path(&path, config.recursive_mode())
                .map_err(|e| (PathOp::Watch(path, config), e)),
            crate::PathOp::Unwatch(path) => self
                .remove_path(&path)
                .map_err(|e| (PathOp::Unwatch(path), e)),
        });

        match self.run() {
            Err(run_error) => match result {
                Ok(()) => Err(crate::UpdatePathsError {
                    source: run_error,
                    origin: None,
                    remaining: Default::default(),
                }),
                Err(path_op_error) => {
                    log::error!(
                        "Unable to run fsevents watcher after updating paths error: {run_error:?}"
                    );
                    Err(path_op_error)
                }
            },
            Ok(()) => result,
        }
    }

    #[inline]
    fn is_running(&self) -> bool {
        self.runloop.is_some()
    }

    fn stop(&mut self) {
        if !self.is_running() {
            return;
        }

        if let Some(RunLoopHandle {
            runloop,
            stop_flag,
            thread_handle,
        }) = self.runloop.take()
        {
            // Don't wait for the runloop to become "waiting" before stopping; if the
            // stream is under heavy load that can delay shutdown indefinitely.
            stop_flag.store(true, Ordering::Release);
            runloop.stop();
            runloop.wake_up();
            // Wait for the thread to shut down.
            thread_handle.join().expect("thread to shut down");
        }
    }

    fn remove_path(&mut self, path: &Path) -> Result<()> {
        let p = path
            .canonicalize()
            .ok()
            .or_else(|| {
                self.watches
                    .iter()
                    .find(|(_, entry)| entry.info.reported_path == path)
                    .map(|(path, _)| path.clone())
            })
            .or_else(|| absolute_path(path).ok())
            .unwrap_or_else(|| path.to_owned());

        match self.watches.remove(&p) {
            Some(_) => Ok(()),
            None => Err(Error::watch_not_found()),
        }
    }

    // https://github.com/thibaudgg/rb-fsevent/blob/master/ext/fsevent_watch/main.c
    fn append_path(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        if !path.exists() {
            return Err(Error::path_not_found().add_path(path.into()));
        }
        let canonical_path = path.to_path_buf().canonicalize()?;
        let mut err: *mut cf::CFError = ptr::null_mut();
        let Some(cf_path) = (unsafe { path_to_cfstring_ref(&canonical_path, &mut err) }) else {
            if let Some(err) = NonNull::new(err) {
                let _ = unsafe { cf::CFRetained::from_raw(err) };
            }
            // Most likely the directory was deleted, or permissions changed,
            // while the above code was running.
            return Err(Error::path_not_found().add_path(path.into()));
        };

        let device = std::fs::metadata(&canonical_path)?.dev();

        self.watches.insert(
            canonical_path,
            WatchEntry {
                info: WatchInfo {
                    is_recursive: recursive_mode.is_recursive(),
                    reported_path: path.to_path_buf(),
                },
                cf_path,
                device,
            },
        );
        Ok(())
    }

    // A recursive watch covers nested watches on the same volume. Non-recursive
    // ancestors may filter out deeper events, and FSEvents may not cross mounts.
    fn stream_paths(&self) -> cf::CFRetained<cf::CFMutableArray<cf::CFString>> {
        let paths: cf::CFRetained<cf::CFMutableArray<cf::CFString>> = cf::CFMutableArray::empty();
        for (path, entry) in &self.watches {
            let covered = path.ancestors().skip(1).any(|ancestor| {
                self.watches.get(ancestor).is_some_and(|covering| {
                    covering.info.is_recursive && covering.device == entry.device
                })
            });
            if !covered {
                paths.append(&entry.cf_path);
            }
        }
        paths
    }

    fn run(&mut self) -> Result<()> {
        let stream_paths = self.stream_paths();
        if stream_paths.is_empty() {
            return Ok(());
        }

        // Over roughly RLIMIT_NOFILE/10 paths across all live streams, FSEvents
        // closes fd 0, which this process owns. The corruption then surfaces as
        // EBADF on unrelated files.
        let path_count = stream_paths.iter().count();
        let budget = fsevents_path_budget().unwrap_or(usize::MAX);
        let path_reservation =
            match FseventsPathReservation::acquire(&ACTIVE_FSEVENTS_PATHS, path_count, budget) {
                Ok(reservation) => reservation,
                Err(active_path_count) => {
                    let combined_path_count = active_path_count.saturating_add(path_count);
                    log::error!(
                        "refusing FSEvents stream: {combined_path_count} active paths exceed the \
                         safe limit of {budget}. Raise RLIMIT_NOFILE, watch fewer paths, or use \
                         macos_kqueue."
                    );
                    return Err(Error::new(ErrorKind::MaxFilesWatch));
                }
            };

        // We need to associate the stream context with our callback in order to propagate events
        // to the rest of the system. This will be owned by the stream, and will be freed when the
        // stream is closed. This means we will leak the context if we panic before reaching
        // `FSEventStreamRelease`.
        let context = Box::into_raw(Box::new(StreamContextInfo {
            event_handler: self.event_handler.clone(),
            recursive_info: self
                .watches
                .iter()
                .map(|(path, entry)| (path.clone(), entry.info.clone()))
                .collect(),
            event_kinds: self.event_kinds,
        }));

        let stream_context = fs::FSEventStreamContext {
            version: 0,
            info: context as *mut libc::c_void,
            retain: None,
            release: Some(release_context),
            copyDescription: None,
        };

        let stream = unsafe {
            fs::FSEventStreamCreate(
                cf::kCFAllocatorDefault,
                Some(callback),
                &stream_context as *const _ as *mut _,
                stream_paths.as_opaque(),
                self.since_when,
                self.latency,
                self.flags,
            )
        };

        // Wrapper to help send CFRunLoop types across threads.
        struct CFRunLoopSendWrapper(cf::CFRetained<cf::CFRunLoop>);

        // Safety:
        // - According to the Apple documentation, it's safe to move `CFRunLoop`s across threads.
        //   https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Multithreading/ThreadSafetySummary/ThreadSafetySummary.html
        unsafe impl Send for CFRunLoopSendWrapper {}

        // Wrapper to help send FSEventStreamRef types across threads.
        struct FSEventStreamSendWrapper(fs::FSEventStreamRef);

        // TODO: Write docs for the safety of this impl.
        // SAFETY: Unclear?
        unsafe impl Send for FSEventStreamSendWrapper {}

        // move into thread
        let stream = FSEventStreamSendWrapper(stream);

        // channel to pass runloop around
        let (rl_tx, rl_rx) = unbounded();

        // Used to stop the runloop thread without relying on privileged APIs or
        // on `CFRunLoopIsWaiting()` becoming true under heavy event load.
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_thread = Arc::clone(&stop_flag);

        let thread_handle = thread::Builder::new()
            .name("notify-rs fsevents loop".to_string())
            .spawn(move || {
                // Keep the shared path count reserved until this stream is released.
                let _path_reservation = path_reservation;
                let _ = &stream;
                let stream = stream.0;

                unsafe {
                    // Safety:
                    // This may panic if OOM occurs.
                    // Related: https://github.com/madsmtm/objc2/issues/797
                    let cur_runloop =
                        cf::CFRunLoop::current().expect("Failed to get current runloop");

                    #[allow(deprecated)]
                    fs::FSEventStreamScheduleWithRunLoop(
                        stream,
                        &cur_runloop,
                        cf::kCFRunLoopDefaultMode.expect("Failed to get default runloop mode"),
                    );
                    if !fs::FSEventStreamStart(stream) {
                        fs::FSEventStreamInvalidate(stream);
                        fs::FSEventStreamRelease(stream);
                        rl_tx
                            .send(Err(Error::generic("unable to start FSEvent stream")))
                            .expect("Unable to send error for FSEventStreamStart");
                        return;
                    }

                    // `stop()` will call `CFRunLoopStop` + `CFRunLoopWakeUp` and then join this
                    // thread.
                    rl_tx
                        .send(Ok(CFRunLoopSendWrapper(cur_runloop)))
                        .expect("Unable to send runloop to watcher");

                    // Avoid polling the runloop: block indefinitely until `CFRunLoopStop` is
                    // called (or until the runloop is otherwise finished).
                    if !stop_flag_thread.load(Ordering::Acquire) {
                        cf::CFRunLoop::run();
                    }
                    fs::FSEventStreamStop(stream);
                    fs::FSEventStreamInvalidate(stream);
                    fs::FSEventStreamRelease(stream);
                }
            })?;
        // block until runloop has been sent
        let runloop_wrapper = match rl_rx.recv() {
            Ok(Ok(runloop_wrapper)) => runloop_wrapper,
            Ok(Err(err)) => {
                thread_handle
                    .join()
                    .expect("thread to shut down after FSEvent stream startup failure");
                return Err(err);
            }
            Err(_) => {
                thread_handle
                    .join()
                    .expect("thread to shut down after FSEvent stream startup channel close");
                return Err(Error::generic(
                    "unable to receive FSEvent stream startup result",
                ));
            }
        };
        self.runloop = Some(RunLoopHandle {
            runloop: runloop_wrapper.0,
            stop_flag,
            thread_handle,
        });

        Ok(())
    }

    fn configure_raw_mode(&mut self, _config: Config, tx: Sender<Result<bool>>) {
        tx.send(Ok(false))
            .expect("configuration channel disconnect");
    }
}

// A twelfth rather than a tenth: the edge also shifts with how many descriptors
// the process already holds.
fn fsevents_path_budget() -> Option<usize> {
    let mut limit = unsafe { std::mem::zeroed::<libc::rlimit>() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return None;
    }
    let soft = usize::try_from(limit.rlim_cur).ok()?;
    Some(soft / 12)
}

unsafe extern "C-unwind" fn callback(
    stream_ref: fs::ConstFSEventStreamRef,
    info: *mut libc::c_void,
    num_events: libc::size_t,                          // size_t numEvents
    event_paths: NonNull<libc::c_void>,                // void *eventPaths
    event_flags: NonNull<fs::FSEventStreamEventFlags>, // const FSEventStreamEventFlags eventFlags[]
    event_ids: NonNull<fs::FSEventStreamEventId>,      // const FSEventStreamEventId eventIds[]
) {
    // Never unwind into CoreServices; if something goes wrong, drop the events and log.
    // This also protects against panics from user-provided `EventHandler` implementations.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        callback_impl(
            stream_ref,
            info,
            num_events,
            event_paths,
            event_flags,
            event_ids,
        )
    }))
    .map_err(|_| {
        log::error!("panic in FSEvents callback; dropping pending events");
    });
}

unsafe fn callback_impl(
    _stream_ref: fs::ConstFSEventStreamRef,
    info: *mut libc::c_void,
    num_events: libc::size_t,                          // size_t numEvents
    event_paths: NonNull<libc::c_void>,                // void *eventPaths
    event_flags: NonNull<fs::FSEventStreamEventFlags>, // const FSEventStreamEventFlags eventFlags[]
    _event_ids: NonNull<fs::FSEventStreamEventId>,     // const FSEventStreamEventId eventIds[]
) {
    let event_paths = event_paths.as_ptr() as *const *const libc::c_char;
    let info = info as *const StreamContextInfo;
    let event_handler_mutex = &(*info).event_handler;
    let event_kinds = (*info).event_kinds;
    let mut event_handler_guard = None;

    for p in 0..num_events {
        // Paths are not guaranteed to be valid UTF-8 (e.g. NFS); keep them as raw bytes.
        let path = CStr::from_ptr(*event_paths.add(p));
        let path = Path::new(OsStr::from_bytes(path.to_bytes()));

        let raw_flag = *event_flags.as_ptr().add(p) as u32;
        let flag = StreamFlags::from_bits_truncate(raw_flag);
        let unknown_bits = raw_flag & !StreamFlags::all().bits();
        if unknown_bits != 0 {
            // `FSEventStreamEventFlags` is an extensible bitfield; tolerate future flags.
            log::trace!("unknown FSEventStreamEventFlags bits: 0x{unknown_bits:08x}");
        }

        let mut watch_match = None;
        for (watch_path, watch_info) in &(*info).recursive_info {
            if path.starts_with(watch_path) {
                let matches_watch = if watch_info.is_recursive || path == watch_path {
                    true
                } else if let Some(parent_path) = path.parent() {
                    parent_path == watch_path
                } else {
                    false
                };

                if matches_watch
                    && watch_match.as_ref().is_none_or(
                        |(matched_path, _): &(&PathBuf, &WatchInfo)| {
                            watch_path.as_os_str().as_bytes().len()
                                > matched_path.as_os_str().as_bytes().len()
                        },
                    )
                {
                    watch_match = Some((watch_path, watch_info));
                }
            }
        }

        let Some((watch_path, watch_info)) = watch_match else {
            continue;
        };
        let translated_count = translated_event_count(&flag, true);
        if translated_count == 0 {
            continue;
        }
        // Most FSEvents flags produce one Event; move the reported path in that case.
        let mut event_path = Some(reported_path(watch_path, &watch_info.reported_path, path));
        let single_translated_event = translated_count == 1;

        log::trace!("FSEvent: path = `{}`, flag = {:?}", path.display(), flag);

        translate_flags_with(flag, true, |mut ev| {
            // Filter events based on EventKindMask before adding the path.
            if !event_kinds.matches(&ev.kind) {
                return;
            }
            if single_translated_event {
                ev.paths.push(
                    event_path.take().unwrap_or_else(|| {
                        reported_path(watch_path, &watch_info.reported_path, path)
                    }),
                );
            } else {
                ev.paths
                    .push(event_path.as_ref().expect("translated event path").clone());
            }

            let event_handler =
                event_handler_guard.get_or_insert_with(|| match event_handler_mutex.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                });
            // Protect against panicking event handlers, which would otherwise unwind into
            // the CoreServices callback.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                event_handler.handle_event(Ok(ev));
            }))
            .map_err(|_| {
                log::error!("panic in FSEvents event handler; dropping event");
            });
        });
    }
}

impl Watcher for FsEventWatcher {
    /// Create a new watcher.
    fn new<F: EventHandler>(event_handler: F, config: Config) -> Result<Self> {
        Self::from_event_handler(
            Arc::new(Mutex::new(event_handler)),
            config.event_kinds(),
            config.fsevent_latency().as_secs_f64(),
        )
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.watch_inner(path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    fn update_paths(&mut self, ops: Vec<PathOp>) -> crate::StdResult<(), crate::UpdatePathsError> {
        self.update_paths_inner(ops)
    }

    fn configure(&mut self, config: Config) -> Result<bool> {
        let (tx, rx) = unbounded();
        self.configure_raw_mode(config, tx);
        rx.recv()?
    }

    fn watched_paths(&self) -> Result<Vec<(PathBuf, RecursiveMode)>> {
        // Unlike the channel-based backends, FSEvents keeps watch state on the watcher itself.
        // The runloop callback gets a cloned snapshot in `StreamContextInfo`, so it does not
        // mutate or read this map concurrently.
        Ok(self
            .watches
            .iter()
            .map(|(_path, WatchEntry { info, .. })| {
                (
                    info.reported_path.clone(),
                    if info.is_recursive {
                        RecursiveMode::Recursive
                    } else {
                        RecursiveMode::NonRecursive
                    },
                )
            })
            .collect())
    }

    fn kind() -> crate::WatcherKind {
        crate::WatcherKind::Fsevent
    }
}

impl Drop for FsEventWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Grabbed from <https://docs.rs/fsevent-sys/4.1.0/src/fsevent_sys/core_foundation.rs.html#149-230>.
///
/// TODO: Could we simplify this?
unsafe fn path_to_cfstring_ref(
    source: &Path,
    err: &mut *mut cf::CFError,
) -> Option<cf::CFRetained<cf::CFString>> {
    let url = cf::CFURL::from_file_path(source)?;

    let mut placeholder = url.absolute_url()?;

    let imaginary = cf::CFMutableArray::empty();

    while !unsafe { placeholder.resource_is_reachable(err) } {
        if let Some(child) = placeholder.last_path_component() {
            imaginary.insert(0, &*child);
        }

        placeholder = cf::CFURL::new_copy_deleting_last_path_component(None, Some(&placeholder))?;
    }

    let url = unsafe { cf::CFURL::new_file_reference_url(None, Some(&placeholder), err) }?;

    let mut placeholder = unsafe { cf::CFURL::new_file_path_url(None, Some(&url), err) }?;

    for component in imaginary {
        placeholder = cf::CFURL::new_copy_appending_path_component(
            None,
            Some(&placeholder),
            Some(&component),
            false,
        )?;
    }

    placeholder.file_system_path(cf::CFURLPathStyle::CFURLPOSIXPathStyle)
}


