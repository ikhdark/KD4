//! Inotify watcher implementation for Linux, Android, and FreeBSD 14.5+.
//!
//! The inotify API provides a mechanism for monitoring filesystem events.  Inotify can be used to
//! monitor individual files, or to monitor directories.  When a directory is monitored, inotify
//! will return events for the directory itself, and for files inside the directory.

use super::event::*;
use super::{
    Config, Error, ErrorKind, EventHandler, RecursiveMode, Result, WatchPathConfig, Watcher,
};
use crate::paths::{
    WatchMetadata, WatchPath, absolute_path, is_preserved_watch_root, preserved_watch_mode,
    preserved_watch_roots, recursive_user_watch_ancestor, reported_path,
};
use crate::{BoundSender, Receiver, Sender, bounded, unbounded};
use inotify as inotify_sys;
use inotify_sys::{EventMask, Inotify, WatchDescriptor, WatchMask};
use notify_types::event::EventKindMask;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{Metadata, metadata, symlink_metadata};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use walkdir::WalkDir;

const INOTIFY: mio::Token = mio::Token(0);
const MESSAGE: mio::Token = mio::Token(1);

/// Flags for a single `inotify_add_watch` call, not event kinds. Never store them in
/// [`Watch::watch_mask`], which is merged into later adds for the same path.
const RESOLUTION_FLAGS: WatchMask = WatchMask::DONT_FOLLOW
    .union(WatchMask::MASK_ADD)
    .union(WatchMask::MASK_CREATE)
    .union(WatchMask::ONLYDIR);

/// Convert an EventKindMask to the corresponding inotify WatchMask.
///
/// When `is_recursive` is true, CREATE and MOVED_TO are always included
/// to enable tracking of newly created subdirectories.
fn event_kind_mask_to_watch_mask(mask: EventKindMask, is_recursive: bool) -> WatchMask {
    let mut watch_mask = WatchMask::empty();

    if is_recursive {
        watch_mask |= WatchMask::CREATE | WatchMask::MOVED_TO;
    }

    if mask.intersects(EventKindMask::CREATE) {
        watch_mask |= WatchMask::CREATE | WatchMask::MOVED_TO;
    }

    if mask.intersects(EventKindMask::REMOVE) {
        watch_mask |= WatchMask::DELETE | WatchMask::MOVED_FROM;
    }

    if mask.intersects(EventKindMask::MODIFY_DATA) {
        // Note: CLOSE_WRITE is intentionally NOT included here because it generates
        // Access(Close(Write)) events, not Modify events. Users who want CLOSE_WRITE
        // events should use ACCESS_CLOSE.
        watch_mask |= WatchMask::MODIFY;
    }

    if mask.intersects(EventKindMask::MODIFY_META) {
        watch_mask |= WatchMask::ATTRIB;
    }

    if mask.intersects(EventKindMask::MODIFY_NAME) {
        watch_mask |= WatchMask::MOVE_SELF;
    }

    if mask.intersects(EventKindMask::ACCESS_OPEN) {
        watch_mask |= WatchMask::OPEN;
    }

    if mask.intersects(EventKindMask::ACCESS_CLOSE) {
        watch_mask |= WatchMask::CLOSE_WRITE;
    }

    if mask.intersects(EventKindMask::ACCESS_CLOSE_NOWRITE) {
        watch_mask |= WatchMask::CLOSE_NOWRITE;
    }

    watch_mask
}

// The EventLoop will set up a mio::Poll and use it to wait for the following:
//
// -  messages telling it what to do
//
// -  events telling it that something has happened on one of the watched files.

struct EventLoop {
    running: bool,
    poll: mio::Poll,
    event_loop_waker: Arc<mio::Waker>,
    event_loop_tx: Sender<EventLoopMsg>,
    event_loop_rx: Receiver<EventLoopMsg>,
    inotify: Option<Inotify>,
    event_handler: Box<dyn EventHandler>,
    /// Absolute path -> inotify descriptor and watch metadata.
    watches: HashMap<PathBuf, Watch>,
    paths: HashMap<WatchDescriptor, PathBuf>,
    rename_event: Option<Event>,
    follow_links: bool,
    event_kind_mask: EventKindMask,
}

struct Watch {
    watch_descriptor: WatchDescriptor,
    watch_mask: WatchMask,
    is_dir: bool,
    dereference: bool,
    metadata: WatchMetadata,
}

/// Watcher implementation based on inotify
#[derive(Debug)]
pub struct INotifyWatcher {
    channel: Sender<EventLoopMsg>,
    waker: Arc<mio::Waker>,
}

enum EventLoopMsg {
    AddWatch(WatchPath, WatchPathConfig, Sender<Result<()>>),
    RemoveWatch(PathBuf, Sender<Result<()>>),
    GetWatchedPaths(Sender<Vec<(PathBuf, RecursiveMode)>>),
    Shutdown,
    Configure(Config, BoundSender<Result<bool>>),
}

#[inline]
fn watch_metadata(path: &Path, dereference: bool) -> std::io::Result<Metadata> {
    if dereference {
        metadata(path)
    } else {
        symlink_metadata(path)
    }
}

#[inline]
fn add_watch_by_event(
    path: &PathBuf,
    event: &inotify_sys::Event<&OsStr>,
    watches: &HashMap<PathBuf, Watch>,
    add_watches: &mut Vec<WatchPath>,
) {
    if event.mask.contains(EventMask::ISDIR) {
        if let Some(parent_path) = path.parent() {
            if let Some(watch) = watches.get(parent_path) {
                if watch.metadata.is_recursive {
                    add_watches.push(WatchPath::from_parts(
                        path.to_owned(),
                        reported_path(parent_path, &watch.metadata.reported_path, path),
                    ));
                }
            }
        }
    }
}

/// Queue `path` for removal, if it is watched and `descriptor` still matches its watch.
#[inline]
fn queue_watch_removal(
    path: &Path,
    descriptor: Option<&WatchDescriptor>,
    watches: &HashMap<PathBuf, Watch>,
    remove_watches: &mut BTreeMap<PathBuf, WatchRemoval>,
    removal: WatchRemoval,
) {
    let watched = watches
        .get(path)
        .is_some_and(|watch| descriptor.is_none_or(|wd| &watch.watch_descriptor == wd));
    if watched {
        let pending = remove_watches.entry(path.to_owned()).or_insert(removal);
        *pending = (*pending).max(removal);
    }
}

/// Variant order is load-bearing: coalescing keeps the greater of two pending removals.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum WatchRemoval {
    /// The watch may still be installed, so remove it from inotify as well as local state.
    WithOsCall,
    /// The kernel removed the event's root descriptor, but descendant descriptors may still be
    /// installed if their filesystem objects survived, for example after being moved elsewhere.
    DescriptorAlreadyRemoved,
}

#[inline]
fn unmount_event(path: PathBuf) -> Event {
    Event::new(EventKind::Remove(RemoveKind::Other))
        .add_path(path)
        .set_info("unmount")
}

impl EventLoop {
    pub fn new(
        inotify: Inotify,
        event_handler: Box<dyn EventHandler>,
        config: &Config,
    ) -> Result<Self> {
        let (event_loop_tx, event_loop_rx) = unbounded::<EventLoopMsg>();
        let poll = mio::Poll::new()?;

        let event_loop_waker = Arc::new(mio::Waker::new(poll.registry(), MESSAGE)?);

        let inotify_fd = inotify.as_raw_fd();
        let mut evented_inotify = mio::unix::SourceFd(&inotify_fd);
        poll.registry()
            .register(&mut evented_inotify, INOTIFY, mio::Interest::READABLE)?;

        let event_loop = EventLoop {
            running: true,
            poll,
            event_loop_waker,
            event_loop_tx,
            event_loop_rx,
            inotify: Some(inotify),
            event_handler,
            watches: HashMap::new(),
            paths: HashMap::new(),
            rename_event: None,
            follow_links: config.follow_symlinks(),
            event_kind_mask: config.event_kinds(),
        };
        Ok(event_loop)
    }

    // Run the event loop.
    pub fn run(self) {
        let _ = thread::Builder::new()
            .name("notify-rs inotify loop".to_string())
            .spawn(|| self.event_loop_thread());
    }

    fn event_loop_thread(mut self) {
        let mut events = mio::Events::with_capacity(16);
        loop {
            // Wait for something to happen.
            match self.poll.poll(&mut events, None) {
                Err(ref e) if matches!(e.kind(), std::io::ErrorKind::Interrupted) => {
                    // System call was interrupted, we will retry
                    // TODO: Not covered by tests (to reproduce likely need to setup signal handlers)
                }
                Err(e) => {
                    self.event_handler.handle_event(Err(Error::io(e)));
                    break;
                }
                Ok(()) => {}
            }

            // Process whatever happened.
            for event in &events {
                self.handle_event(event);
            }

            // Stop, if we're done.
            if !self.running {
                break;
            }
        }
    }

    // Handle a single event.
    fn handle_event(&mut self, event: &mio::event::Event) {
        match event.token() {
            MESSAGE => {
                // The channel is readable - handle messages.
                self.handle_messages()
            }
            INOTIFY => {
                // inotify has something to tell us.
                self.handle_inotify()
            }
            _ => unreachable!(),
        }
    }

    fn handle_messages(&mut self) {
        while let Ok(msg) = self.event_loop_rx.try_recv() {
            match msg {
                EventLoopMsg::AddWatch(path, config, tx) => {
                    let _ = tx.send(self.add_watch(path, config, true));
                }
                EventLoopMsg::RemoveWatch(path, tx) => {
                    let _ = tx.send(self.remove_watch(path, false));
                }
                EventLoopMsg::GetWatchedPaths(tx) => {
                    let _ = tx.send(
                        self.watches
                            .iter()
                            .filter(|(_path, watch)| watch.metadata.is_user_watch)
                            .map(|(_path, watch)| {
                                (
                                    watch.metadata.reported_path.clone(),
                                    if watch.metadata.user_is_recursive {
                                        RecursiveMode::Recursive
                                    } else {
                                        RecursiveMode::NonRecursive
                                    },
                                )
                            })
                            .collect(),
                    );
                }
                EventLoopMsg::Shutdown => {
                    let _ = self.remove_all_watches();
                    if let Some(inotify) = self.inotify.take() {
                        let _ = inotify.close();
                    }
                    self.running = false;
                    break;
                }
                EventLoopMsg::Configure(config, tx) => {
                    self.configure_raw_mode(config, tx);
                }
            }
        }
    }

    fn configure_raw_mode(&mut self, _config: Config, tx: BoundSender<Result<bool>>) {
        tx.send(Ok(false))
            .expect("configuration channel disconnected");
    }

    fn handle_inotify(&mut self) {
        let mut add_watches = Vec::new();
        let mut remove_watches = BTreeMap::new();

        if let Some(ref mut inotify) = self.inotify {
            let mut buffer = [0; 1024];
            // Read all buffers available.
            loop {
                match inotify.read_events(&mut buffer) {
                    Ok(events) => {
                        let mut num_events = 0;
                        for event in events {
                            log::trace!("inotify event: {event:?}");

                            num_events += 1;
                            if event.mask.contains(EventMask::Q_OVERFLOW) {
                                let ev = Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan));
                                self.event_handler.handle_event(ev);
                            }

                            if event.mask.contains(EventMask::IGNORED) {
                                // The kernel sends IGNORED whenever it removes a descriptor,
                                // regardless of the configured mask. A replacement watch may
                                // already occupy the same pathname, hence the descriptor check.
                                if let Some(path) = self.paths.get(&event.wd).cloned() {
                                    if self
                                        .watches
                                        .get(&path)
                                        .is_some_and(|watch| watch.watch_descriptor == event.wd)
                                    {
                                        queue_watch_removal(
                                            &path,
                                            None,
                                            &self.watches,
                                            &mut remove_watches,
                                            WatchRemoval::DescriptorAlreadyRemoved,
                                        );
                                    } else {
                                        self.paths.remove(&event.wd);
                                    }
                                }
                                continue;
                            }

                            let paths = self.paths.get(&event.wd).and_then(|root| {
                                self.watches.get(root).map(|watch| match event.name {
                                    Some(name) => {
                                        let path = root.join(name);
                                        let reported_path = reported_path(
                                            root,
                                            &watch.metadata.reported_path,
                                            &path,
                                        );
                                        (path, reported_path)
                                    }
                                    None => (root.clone(), watch.metadata.reported_path.clone()),
                                })
                            });

                            let (path, event_path) = match paths {
                                Some(paths) => paths,
                                None => {
                                    log::debug!("inotify event with unknown descriptor: {event:?}");
                                    continue;
                                }
                            };

                            let mut evs = Vec::new();

                            if event.mask.contains(EventMask::MOVED_FROM) {
                                queue_watch_removal(
                                    &path,
                                    None,
                                    &self.watches,
                                    &mut remove_watches,
                                    WatchRemoval::WithOsCall,
                                );

                                let event = Event::new(EventKind::Modify(ModifyKind::Name(
                                    RenameMode::From,
                                )))
                                .add_path(event_path.clone())
                                .set_tracker(event.cookie as usize);

                                self.rename_event = Some(event.clone());

                                evs.push(event);
                            } else if event.mask.contains(EventMask::MOVED_TO) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To)))
                                        .set_tracker(event.cookie as usize)
                                        .add_path(event_path.clone()),
                                );

                                let trackers_match =
                                    self.rename_event.as_ref().and_then(|e| e.tracker())
                                        == Some(event.cookie as usize);

                                if trackers_match {
                                    let rename_event = self.rename_event.take().unwrap(); // unwrap is safe because `rename_event` must be set at this point
                                    evs.push(
                                        Event::new(EventKind::Modify(ModifyKind::Name(
                                            RenameMode::Both,
                                        )))
                                        .set_tracker(event.cookie as usize)
                                        .add_some_path(rename_event.paths.first().cloned())
                                        .add_path(event_path.clone()),
                                    );
                                }
                                add_watch_by_event(&path, &event, &self.watches, &mut add_watches);
                            }
                            if event.mask.contains(EventMask::MOVE_SELF) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Name(
                                        RenameMode::From,
                                    )))
                                    .add_path(event_path.clone()),
                                );
                                // TODO stat the path and get to new path
                                // - emit To and Both events
                                // - change prefix for further events
                            }
                            if event.mask.contains(EventMask::CREATE) {
                                evs.push(
                                    Event::new(EventKind::Create(
                                        if event.mask.contains(EventMask::ISDIR) {
                                            CreateKind::Folder
                                        } else {
                                            CreateKind::File
                                        },
                                    ))
                                    .add_path(event_path.clone()),
                                );
                                add_watch_by_event(&path, &event, &self.watches, &mut add_watches);
                            }
                            if event.mask.contains(EventMask::DELETE) {
                                evs.push(
                                    Event::new(EventKind::Remove(
                                        if event.mask.contains(EventMask::ISDIR) {
                                            RemoveKind::Folder
                                        } else {
                                            RemoveKind::File
                                        },
                                    ))
                                    .add_path(event_path.clone()),
                                );
                                queue_watch_removal(
                                    &path,
                                    None,
                                    &self.watches,
                                    &mut remove_watches,
                                    WatchRemoval::WithOsCall,
                                );
                            }
                            if event.mask.contains(EventMask::DELETE_SELF) {
                                let remove_kind = match self.watches.get(&path) {
                                    Some(watch) if watch.is_dir => RemoveKind::Folder,
                                    Some(_) => RemoveKind::File,
                                    None => RemoveKind::Other,
                                };
                                evs.push(
                                    Event::new(EventKind::Remove(remove_kind))
                                        .add_path(event_path.clone()),
                                );
                                // Deleting a watched inode removes its watch in the kernel and
                                // queues IGNORED, so calling inotify_rm_watch would return EINVAL.
                                queue_watch_removal(
                                    &path,
                                    Some(&event.wd),
                                    &self.watches,
                                    &mut remove_watches,
                                    WatchRemoval::DescriptorAlreadyRemoved,
                                );
                            }
                            if event.mask.contains(EventMask::UNMOUNT) {
                                evs.push(unmount_event(event_path.clone()));
                                // The kernel has already removed this watch descriptor and will
                                // emit IGNORED; clean up internal state without inotify_rm_watch.
                                // ref. https://www.man7.org/linux/man-pages/man7/inotify.7.html
                                queue_watch_removal(
                                    &path,
                                    Some(&event.wd),
                                    &self.watches,
                                    &mut remove_watches,
                                    WatchRemoval::DescriptorAlreadyRemoved,
                                );
                            }
                            if event.mask.contains(EventMask::MODIFY) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Data(
                                        DataChange::Any,
                                    )))
                                    .add_path(event_path.clone()),
                                );
                            }
                            if event.mask.contains(EventMask::CLOSE_WRITE) {
                                evs.push(
                                    Event::new(EventKind::Access(AccessKind::Close(
                                        AccessMode::Write,
                                    )))
                                    .add_path(event_path.clone()),
                                );
                            }
                            if event.mask.contains(EventMask::CLOSE_NOWRITE) {
                                evs.push(
                                    Event::new(EventKind::Access(AccessKind::Close(
                                        AccessMode::Read,
                                    )))
                                    .add_path(event_path.clone()),
                                );
                            }
                            if event.mask.contains(EventMask::ATTRIB) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Metadata(
                                        MetadataKind::Any,
                                    )))
                                    .add_path(event_path.clone()),
                                );
                            }
                            if event.mask.contains(EventMask::OPEN) {
                                evs.push(
                                    Event::new(EventKind::Access(AccessKind::Open(
                                        AccessMode::Any,
                                    )))
                                    .add_path(event_path.clone()),
                                );
                            }

                            // Filter events based on EventKindMask before delivery
                            for ev in evs {
                                if self.event_kind_mask.matches(&ev.kind) {
                                    self.event_handler.handle_event(Ok(ev));
                                }
                            }
                        }

                        // All events read. Break out.
                        if num_events == 0 {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // No events read. Break out.
                        break;
                    }
                    Err(e) => {
                        self.event_handler.handle_event(Err(Error::io(e)));
                    }
                }
            }
        }

        // An ancestor sorts before its descendants, so reverse order removes descendants first:
        // recursive ancestor cleanup then neither repeats syscalls for kernel-removed watches nor
        // drops the mapping for a live, moved-out descendant.
        for (path, removal) in remove_watches.into_iter().rev() {
            if !self.watches.contains_key(&path) {
                continue;
            }

            let result = match removal {
                WatchRemoval::WithOsCall => self.remove_watch(path, true),
                WatchRemoval::DescriptorAlreadyRemoved => {
                    self.remove_watch_without_root_os_call(path, true)
                }
            };
            if let Err(err) = result {
                log::warn!("Unable to remove the path from the watches: {err:?}");
            }
        }

        for path in add_watches {
            let config = WatchPathConfig::new(RecursiveMode::Recursive);
            if let Err(add_watch_error) = self.add_watch(path, config, false) {
                // The handler should be notified if we have reached the limit.
                // Otherwise, the user might expect that a recursive watch
                // is continuing to work correctly, but it's not.
                if let ErrorKind::MaxFilesWatch = add_watch_error.kind {
                    self.event_handler.handle_event(Err(add_watch_error));

                    // After that kind of a error we should stop adding watches,
                    // because the limit has already reached and all next calls
                    // will return us only the same error.
                    break;
                }
            }
        }
    }

    fn add_watch(
        &mut self,
        path: WatchPath,
        config: WatchPathConfig,
        watch_self: bool,
    ) -> Result<()> {
        let is_recursive = config.recursive_mode().is_recursive();
        // a recursive watch has to resolve the path before it can walk it
        let dereference = config.dereference_symlinks() || is_recursive;
        let path_is_dir = watch_metadata(&path.absolute, dereference)
            .map_err(Error::io_watch)?
            .is_dir();
        let requested_is_recursive = is_recursive && path_is_dir;
        if watch_self {
            if let Some(watch) = self
                .watches
                .get(&path.absolute)
                .filter(|watch| watch.metadata.is_user_watch)
            {
                if watch.metadata.user_is_recursive == requested_is_recursive
                    && watch.metadata.reported_path == path.requested
                    && watch.dereference == dereference
                {
                    return Ok(());
                }

                // Rewatching an explicit user watch replaces its requested mode and reported path
                // instead of merging with the previous metadata. If the current entry also carries
                // recursive coverage from an ancestor, remember that ancestor before removal so we
                // can rebuild that inherited coverage below.
                let inherited_recursive_root =
                    if !requested_is_recursive && path_is_dir && watch.metadata.is_recursive {
                        recursive_user_watch_ancestor(
                            &path.absolute,
                            self.watches
                                .iter()
                                .map(|(path, watch)| (path, &watch.metadata)),
                        )
                    } else {
                        None
                    };
                let replaced_path = path.absolute.clone();
                self.remove_watch(replaced_path.clone(), false)?;

                if let Some((ancestor_path, ancestor_reported_path)) = inherited_recursive_root {
                    // Removing a directory watch removes its recursively inherited children too.
                    // Re-add them as non-user watches so the ancestor recursive watch still covers
                    // this subtree after the user watch is replaced.
                    let entries = recursive_directory_paths(
                        replaced_path.clone(),
                        self.follow_links,
                        self.recursive_walk_barriers(),
                    )
                    .map(|entry| {
                        let absolute = entry;
                        let requested =
                            reported_path(&ancestor_path, &ancestor_reported_path, &absolute);
                        WatchPath::from_parts(absolute, requested)
                    });
                    self.add_watches_for_paths(entries, true, true, false)?;
                }
            } else if self.watches.get(&path.absolute).is_some_and(|watch| {
                !dereference && !path_is_dir && watch.is_dir && watch.metadata.is_recursive
            }) {
                // A recursive walk already followed this link. Remove its inherited subtree before
                // replacing the root with an explicit watch on the link itself.
                self.remove_watch(path.absolute.clone(), false)?;
            }
        }

        // If the watch is not recursive, or if we determine (by stat'ing the path to get its
        // metadata) that the watched path is not a directory, add a single path watch.
        if !requested_is_recursive {
            return self.add_single_watch(path, false, dereference, true);
        }

        let root = path.clone();
        let entries = recursive_directory_paths(
            root.absolute.clone(),
            self.follow_links,
            self.recursive_walk_barriers(),
        )
        .map(move |entry| root.child(entry));

        self.add_watches_for_paths(entries, is_recursive, dereference, watch_self)
    }

    fn recursive_walk_barriers(&self) -> HashSet<PathBuf> {
        self.watches
            .iter()
            .filter(|(_, watch)| {
                watch.metadata.is_user_watch && !watch.dereference && !watch.is_dir
            })
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn add_watches_for_paths<I>(
        &mut self,
        paths: I,
        is_recursive: bool,
        dereference: bool,
        mut watch_self: bool,
    ) -> Result<()>
    where
        I: IntoIterator<Item = WatchPath>,
    {
        for path in paths {
            // entries below the root were reached by following links, so they observe what they
            // resolved to
            let entry_dereference = if watch_self { dereference } else { true };
            match self.add_single_watch(path, is_recursive, entry_dereference, watch_self) {
                Ok(()) => {}
                // TOCTOU: a subdirectory can disappear between walkdir listing it and us adding an
                // inotify watch for it. This should not fail the overall recursive watch call.
                Err(err) if !watch_self && matches!(err.kind, ErrorKind::PathNotFound) => {}
                Err(err) => return Err(err),
            }
            watch_self = false;
        }

        Ok(())
    }

    fn add_single_watch(
        &mut self,
        path: WatchPath,
        is_recursive: bool,
        requested_dereference: bool,
        watch_self: bool,
    ) -> Result<()> {
        // Build watch mask from configured event kinds for kernel-level filtering
        let mut watchmask = event_kind_mask_to_watch_mask(self.event_kind_mask, is_recursive);

        if watch_self {
            watchmask.insert(WatchMask::DELETE_SELF);
            watchmask.insert(WatchMask::MOVE_SELF);
        }

        let existing_watch = self.watches.get(&path.absolute);
        // an explicit watch decides for its own path, a walk must not overrule it #255
        let dereference = if watch_self {
            requested_dereference
        } else {
            existing_watch
                .filter(|watch| watch.metadata.is_user_watch)
                .map_or(requested_dereference, |watch| watch.dereference)
        };
        let previous_descriptor = existing_watch.map(|watch| watch.watch_descriptor.clone());

        let mut add_mask = watchmask;
        if let Some(watch) = existing_watch {
            watchmask.insert(watch.watch_mask);
            add_mask = watchmask | WatchMask::MASK_ADD;
        }
        if !dereference {
            add_mask.insert(WatchMask::DONT_FOLLOW);
        }

        if let Some(ref mut inotify) = self.inotify {
            log::trace!("adding inotify watch: {}", path.absolute.display());

            match inotify.watches().add(&path.absolute, add_mask) {
                Err(e) => {
                    Err(if e.raw_os_error() == Some(libc::ENOSPC) {
                        // do not report inotify limits as "no more space" on linux #266
                        Error::new(ErrorKind::MaxFilesWatch)
                    } else if e.kind() == std::io::ErrorKind::NotFound {
                        Error::new(ErrorKind::PathNotFound)
                    } else {
                        Error::io(e)
                    }
                    .add_path(path.requested))
                }
                Ok(w) => {
                    debug_assert!(!watchmask.intersects(RESOLUTION_FLAGS));
                    let is_dir = match watch_metadata(&path.absolute, dereference) {
                        Ok(metadata) => metadata.is_dir(),
                        Err(e) => {
                            // Avoid leaking an inotify watch if we can't stat after adding it.
                            // This can happen due to racy deletions.
                            let _ = inotify.watches().remove(w.clone());
                            return Err(Error::io_watch(e).add_path(path.requested));
                        }
                    };
                    let metadata = if let Some(existing_watch) = existing_watch {
                        WatchMetadata::new(
                            &path,
                            is_recursive,
                            watch_self,
                            Some(&existing_watch.metadata),
                            self.watches
                                .iter()
                                .map(|(path, watch)| (path, &watch.metadata)),
                        )
                    } else {
                        WatchMetadata {
                            is_recursive,
                            reported_path: path.requested.clone(),
                            is_user_watch: watch_self,
                            user_is_recursive: watch_self && is_recursive,
                        }
                    };

                    // re-resolving a path can land on a different inode, so release the old
                    // descriptor or it keeps reporting under this path
                    if let Some(previous) = previous_descriptor.filter(|previous| *previous != w) {
                        // a walk that followed the link shares this descriptor #255
                        let still_watched = self
                            .watches
                            .iter()
                            .find(|(other, watch)| {
                                *other != &path.absolute && watch.watch_descriptor == previous
                            })
                            .map(|(other, _)| other.clone());
                        match still_watched {
                            Some(other) => {
                                self.paths.insert(previous, other);
                            }
                            None => {
                                self.paths.remove(&previous);
                                Self::remove_single_descriptor(&mut inotify.watches(), previous);
                            }
                        }
                    }

                    self.watches.insert(
                        path.absolute.clone(),
                        Watch {
                            watch_descriptor: w.clone(),
                            watch_mask: watchmask,
                            is_dir,
                            dereference,
                            metadata,
                        },
                    );
                    self.paths.insert(w, path.absolute);
                    Ok(())
                }
            }
        } else {
            Ok(())
        }
    }

    fn remove_watch(&mut self, path: PathBuf, remove_recursive: bool) -> Result<()> {
        let preserved_roots = preserved_watch_roots(
            &path,
            remove_recursive,
            self.watches
                .iter()
                .map(|(path, watch)| (path, &watch.metadata)),
        );

        let watch = self
            .watches
            .remove(&path)
            .ok_or_else(|| Error::watch_not_found().add_path(path.clone()))?;
        log::trace!("removing inotify watch for {path:?}, remove_recursive: {remove_recursive:?}");

        let mut removed_descriptors = vec![watch.watch_descriptor];
        if watch.metadata.is_recursive || remove_recursive {
            let mut remove_list = Vec::new();
            let mut reset_list = Vec::new();
            for candidate in self.watches.keys() {
                if candidate.starts_with(&path) {
                    if let Some(user_is_recursive) =
                        preserved_watch_mode(candidate, &preserved_roots)
                    {
                        if !user_is_recursive
                            || is_preserved_watch_root(candidate, &preserved_roots)
                        {
                            reset_list.push(candidate.clone());
                        }
                        continue;
                    }

                    remove_list.push(candidate.clone());
                }
            }

            for path in remove_list {
                if let Some(watch) = self.watches.remove(&path) {
                    removed_descriptors.push(watch.watch_descriptor);
                }
            }
            for path in reset_list {
                if let Some(watch) = self.watches.get_mut(&path) {
                    watch.metadata.is_recursive = watch.metadata.user_is_recursive;
                }
            }
        }

        self.release_descriptors(removed_descriptors);
        Ok(())
    }

    /// Remove descriptors that no remaining logical path uses, and repoint shared descriptors.
    fn release_descriptors<I>(&mut self, descriptors: I)
    where
        I: IntoIterator<Item = WatchDescriptor>,
    {
        let descriptors: HashSet<_> = descriptors.into_iter().collect();
        let mut remaining_owners = HashMap::new();
        for (path, watch) in &self.watches {
            if descriptors.contains(&watch.watch_descriptor) {
                remaining_owners
                    .entry(watch.watch_descriptor.clone())
                    .or_insert_with(|| path.clone());
            }
        }

        for descriptor in descriptors {
            if let Some(path) = remaining_owners.remove(&descriptor) {
                self.paths.insert(descriptor, path);
                continue;
            }

            self.paths.remove(&descriptor);
            if let Some(ref mut inotify) = self.inotify {
                Self::remove_single_descriptor(&mut inotify.watches(), descriptor);
            }
        }
    }

    /// Remove a root watch after the kernel has already invalidated its descriptor.
    ///
    /// Only the root skips `inotify_rm_watch`. Recursive descendants are removed individually
    /// because their filesystem objects—and therefore their descriptors—may still be live.
    fn remove_watch_without_root_os_call(
        &mut self,
        path: PathBuf,
        remove_recursive: bool,
    ) -> Result<()> {
        let preserved_roots = preserved_watch_roots(
            &path,
            remove_recursive,
            self.watches
                .iter()
                .map(|(path, watch)| (path, &watch.metadata)),
        );

        match self.watches.remove(&path) {
            None => return Err(Error::watch_not_found().add_path(path)),
            Some(watch) => {
                self.paths.remove(&watch.watch_descriptor);

                if watch.metadata.is_recursive || remove_recursive {
                    let mut inotify_watches =
                        self.inotify.as_mut().map(|inotify| inotify.watches());
                    let mut remove_list = Vec::new();
                    let mut reset_list = Vec::new();
                    for (w, p) in &self.paths {
                        if p.starts_with(&path) {
                            if let Some(user_is_recursive) =
                                preserved_watch_mode(p, &preserved_roots)
                            {
                                if !user_is_recursive
                                    || is_preserved_watch_root(p, &preserved_roots)
                                {
                                    reset_list.push(p.clone());
                                }
                                continue;
                            }

                            // The kernel removing the root does not prove that this descendant's
                            // descriptor was removed, for example if it was moved elsewhere.
                            if let Some(inotify_watches) = inotify_watches.as_mut() {
                                Self::remove_single_descriptor(inotify_watches, w.clone());
                            }
                            self.watches.remove(p);
                            remove_list.push(w.clone());
                        }
                    }
                    for w in remove_list {
                        self.paths.remove(&w);
                    }
                    for p in reset_list {
                        if let Some(watch) = self.watches.get_mut(&p) {
                            watch.metadata.is_recursive = watch.metadata.user_is_recursive;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Remove a descriptor while tolerating the deletion race documented by inotify.
    ///
    /// Linux may invalidate a watch before its queued deletion event is handled. In that case
    /// `inotify_rm_watch` returns EINVAL, but the requested end state has already been reached.
    /// Other errors indicate an unexpected descriptor or inotify-instance failure and stay visible.
    fn remove_single_descriptor(watches: &mut inotify::Watches, wd: WatchDescriptor) {
        if let Err(err) = watches.remove(wd) {
            if err.raw_os_error() == Some(libc::EINVAL) {
                log::trace!("watch descriptor was already removed from inotify: {err:?}");
            } else {
                log::warn!("unable to remove watch descriptor from inotify: {err:?}");
            }
        }
    }

    fn remove_all_watches(&mut self) -> Result<()> {
        if let Some(ref mut inotify) = self.inotify {
            let mut inotify_watches = inotify.watches();
            for (w, p) in &self.paths {
                inotify_watches
                    .remove(w.clone())
                    .map_err(|e| Error::io(e).add_path(p.into()))?;
            }
            self.watches.clear();
            self.paths.clear();
        }
        Ok(())
    }
}

/// return `DirEntry` when it is a directory
fn filter_dir(e: walkdir::Result<walkdir::DirEntry>) -> Option<walkdir::DirEntry> {
    if let Ok(e) = e {
        if e.file_type().is_dir() {
            return Some(e);
        }
    }
    None
}

fn recursive_directory_paths(
    root: PathBuf,
    follow_links: bool,
    barriers: HashSet<PathBuf>,
) -> impl Iterator<Item = PathBuf> {
    WalkDir::new(root)
        .follow_links(follow_links)
        .into_iter()
        .filter_entry(move |entry| !barriers.contains(entry.path()))
        .filter_map(filter_dir)
        .map(|entry| entry.into_path())
}

impl INotifyWatcher {
    fn from_event_handler(event_handler: Box<dyn EventHandler>, config: &Config) -> Result<Self> {
        let inotify = Inotify::init()?;
        let event_loop = EventLoop::new(inotify, event_handler, config)?;
        let channel = event_loop.event_loop_tx.clone();
        let waker = event_loop.event_loop_waker.clone();
        event_loop.run();
        Ok(INotifyWatcher { channel, waker })
    }

    fn watch_inner(&mut self, path: &Path, config: WatchPathConfig) -> Result<()> {
        let pb = WatchPath::new(path)?;
        let (tx, rx) = unbounded();
        let msg = EventLoopMsg::AddWatch(pb, config, tx);

        self.channel.send(msg)?;
        self.waker.wake()?;
        rx.recv().map_err(Error::from)?
    }

    fn unwatch_inner(&mut self, path: &Path) -> Result<()> {
        let pb = absolute_path(path)?;
        let (tx, rx) = unbounded();
        let msg = EventLoopMsg::RemoveWatch(pb, tx);

        self.channel.send(msg)?;
        self.waker.wake()?;
        rx.recv().map_err(Error::from)?
    }

    fn watched_paths_inner(&self) -> Result<Vec<(PathBuf, RecursiveMode)>> {
        let (tx, rx) = unbounded();
        self.channel.send(EventLoopMsg::GetWatchedPaths(tx))?;
        self.waker.wake()?;
        rx.recv().map_err(Error::from)
    }
}

impl Watcher for INotifyWatcher {
    /// Create a new watcher.
    fn new<F: EventHandler>(event_handler: F, config: Config) -> Result<Self> {
        Self::from_event_handler(Box::new(event_handler), &config)
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.watch_inner(path, WatchPathConfig::new(recursive_mode))
    }

    fn watch_with(&mut self, path: &Path, config: WatchPathConfig) -> Result<()> {
        self.watch_inner(path, config)
    }

    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    fn configure(&mut self, config: Config) -> Result<bool> {
        let (tx, rx) = bounded(1);
        self.channel.send(EventLoopMsg::Configure(config, tx))?;
        self.waker.wake()?;
        rx.recv()?
    }

    fn watched_paths(&self) -> Result<Vec<(PathBuf, RecursiveMode)>> {
        self.watched_paths_inner()
    }

    fn kind() -> crate::WatcherKind {
        crate::WatcherKind::Inotify
    }
}

impl Drop for INotifyWatcher {
    fn drop(&mut self) {
        // The producer may already have exited after a terminal polling error.
        let _ = self.channel.send(EventLoopMsg::Shutdown);
        let _ = self.waker.wake();
    }
}

