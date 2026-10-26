//! Cross-platform file system notification library
//!
//! # Installation
//!
//! ```toml
//! [dependencies]
//! notify = "9.0.0-rc.5"
//! ```
//!
//! If you want debounced events (or don't need them in-order), see [notify-debouncer-mini](https://docs.rs/notify-debouncer-mini/latest/notify_debouncer_mini/)
//! or [notify-debouncer-full](https://docs.rs/notify-debouncer-full/latest/notify_debouncer_full/).
//!
//! ## Features
//!
//! List of compilation features, see below for details
//!
//! - `serde` for serialization of events
//! - `macos_fsevent` (default) for the FSEvents backend on macOS
//! - `macos_kqueue` for the kqueue backend on macOS
//! - `freebsd_inotify` enables notify's inotify backend on FreeBSD 14.5+
//!   - inotify is automatically enabled when built natively, this feature is only needed for cross-compilation
//! - `serialization-compat-6` restores the serialization behavior of notify 6, off by default
//!
//! Native FreeBSD builds use inotify on 14.5+ and kqueue on older releases. When
//! cross-compiling for FreeBSD 14.5+, enable `freebsd_inotify` explicitly.
//!
//! ### Serde
//!
//! Events are serializable via [serde](https://serde.rs) if the `serde` feature is enabled:
//!
//! ```toml
//! notify = { version = "9.0.0-rc.5", features = ["serde"] }
//! ```
//!
//! # Known Problems
//!
//! ### Network filesystems
//!
//! Network mounted filesystems like NFS may not emit any events for notify to listen to.
//! This applies especially to WSL programs watching windows paths ([issue #254](https://github.com/notify-rs/notify/issues/254)).
//!
//! A workaround is the [`PollWatcher`] backend.
//!
//! ### Docker with Linux on macOS M1
//!
//! Docker on macOS M1 [throws](https://github.com/notify-rs/notify/issues/423) `Function not implemented (os error 38)`.
//! You have to manually use the [`PollWatcher`], as the native backend isn't available inside the emulation.
//!
//! ### macOS, FSEvents and unowned files
//!
//! Due to the inner security model of FSEvents (see [FileSystemEventSecurity](https://developer.apple.com/library/mac/documentation/Darwin/Conceptual/FSEvents_ProgGuide/FileSystemEventSecurity/FileSystemEventSecurity.html)),
//! some events cannot be observed easily when trying to follow files that do not
//! belong to you. In this case, reverting to the pollwatcher can fix the issue,
//! with a slight performance cost.
//!
//! ### macOS/APFS: `std::fs::copy` can trigger events on the source path
//!
//! On APFS, `std::fs::copy` may use copy-on-write cloning (`fclonefileat`/`clonefile`).
//! This can update inode metadata on the source file, and FSEvents may report a metadata change
//! for the source path (see [issue #259](https://github.com/notify-rs/notify/issues/259) and
//! [issue #465](https://github.com/notify-rs/notify/issues/465)).
//!
//! Workarounds are to avoid `std::fs::copy` (use `std::io::copy` or `read`/`write` instead), or
//! filter out metadata-only events if they're not relevant (e.g. don't include
//! `EventKindMask::MODIFY_META` in [`Config::with_event_kinds`]).
//!
//! ### Editor Behaviour
//!
//! If you rely on precise events (Write/Delete/Create..), you will notice that the actual events
//! can differ a lot between file editors. Some truncate the file on save, some create a new one and replace the old one.
//! See also [this](https://github.com/notify-rs/notify/issues/247) and [this](https://github.com/notify-rs/notify/issues/113#issuecomment-281836995) issues for example.
//!
//! ### Parent folder deletion
//!
//! If you want to receive an event for a deletion of folder `b` for the path `/a/b/..`, you will have to watch its parent `/a`.
//! See [here](https://github.com/notify-rs/notify/issues/403) for more details.
//!
//! ### Pseudo Filesystems like /proc, /sys
//!
//! Some filesystems like `/proc` and `/sys` on *nix do not emit change events or use correct file change dates.
//! To circumvent that problem you can use the [`PollWatcher`] with the `compare_contents` option.
//!
//! ### Linux: Bad File Descriptor / No space left on device
//!
//! This may be the case of running into the max-files watched limits of your user or system.
//! (Files also includes folders.) Note that for recursive watched folders each file and folder inside counts towards the limit.
//!
//! You may increase this limit in linux via
//! ```sh
//! sudo sysctl fs.inotify.max_user_instances=8192 # example number
//! sudo sysctl fs.inotify.max_user_watches=524288 # example number
//! sudo sysctl -p
//! ```
//!
//! Note that the [`PollWatcher`] is not restricted by this limitation, so it may be an alternative if your users can't increase the limit.
//!
//! ### Watching large directories
//!
//! When watching a very large amount of files, notify may fail to receive all events.
//! For example the linux backend is documented to not be a 100% reliable source. See also issue [#412](https://github.com/notify-rs/notify/issues/412).
//!
//! # Examples
//!
//! For more examples visit the [examples folder](https://github.com/notify-rs/notify/tree/main/examples) in the repository.
//!
//! ```text
//! use notify::{Event, RecursiveMode, Result, Watcher};
//! use std::{path::Path, sync::mpsc};
//!
//! fn main() -> Result<()> {
//!     let (tx, rx) = mpsc::channel::<Result<Event>>();
//!
//!     // Use recommended_watcher() to automatically select the best implementation
//!     // for your platform. The `EventHandler` passed to this constructor can be a
//!     // closure, a `std::sync::mpsc::Sender`, a `crossbeam_channel::Sender`, or
//!     // another type the trait is implemented for.
//!     let mut watcher = notify::recommended_watcher(tx)?;
//!
//!     // Add a path to be watched. All files and directories at that path and
//!     // below will be monitored for changes.
//! #     #[cfg(not(any(
//! #     target_os = "freebsd",
//! #     target_os = "openbsd",
//! #     target_os = "dragonfly",
//! #     target_os = "netbsd")))]
//! #     { // "." doesn't exist on BSD for some reason in CI
//!     watcher.watch(Path::new("."), RecursiveMode::Recursive)?;
//! #     }
//! #     #[cfg(any())]
//! #     { // don't run this in doctests, it blocks forever
//!     // Block forever, printing out events as they come in
//!     for res in rx {
//!         match res {
//!             Ok(event) => println!("event: {:?}", event),
//!             Err(e) => println!("watch error: {:?}", e),
//!         }
//!     }
//! #     }
//!
//!     Ok(())
//! }
//! ```
//!
//! ## With different configurations
//!
//! It is possible to create several watchers with different configurations or implementations that
//! all call the same event function. This can accommodate advanced behaviour or work around limits.
//!
//! ```text
//! # use notify::{RecursiveMode, Result, Watcher};
//! # use std::path::Path;
//! #
//! # fn main() -> Result<()> {
//!       fn event_fn(res: Result<notify::Event>) {
//!           match res {
//!              Ok(event) => println!("event: {:?}", event),
//!              Err(e) => println!("watch error: {:?}", e),
//!           }
//!       }
//!
//!       let mut watcher1 = notify::recommended_watcher(event_fn)?;
//!       // we will just use the same watcher kind again here
//!       let mut watcher2 = notify::recommended_watcher(event_fn)?;
//! #     #[cfg(not(any(
//! #     target_os = "freebsd",
//! #     target_os = "openbsd",
//! #     target_os = "dragonfly",
//! #     target_os = "netbsd")))]
//! #     { // "." doesn't exist on BSD for some reason in CI
//! #     watcher1.watch(Path::new("."), RecursiveMode::Recursive)?;
//! #     watcher2.watch(Path::new("."), RecursiveMode::Recursive)?;
//! #     }
//!       // dropping the watcher1/2 here (no loop etc) will end the program
//! #
//! #     Ok(())
//! # }
//! ```

#![deny(missing_docs)]

pub use config::{Config, PathOp, RecursiveMode, WatchPathConfig, WindowsPathSeparatorStyle};
pub use error::{Error, ErrorKind, Result, UpdatePathsError};
pub use notify_types::event::{self, Event, EventKind, EventKindMask};
use std::path::{Path, PathBuf};

pub(crate) type StdResult<T, E> = std::result::Result<T, E>;
pub(crate) type Receiver<T> = std::sync::mpsc::Receiver<T>;
pub(crate) type Sender<T> = std::sync::mpsc::Sender<T>;
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    all(target_os = "freebsd", notify_freebsd_inotify),
    target_os = "windows",
))]
pub(crate) type BoundSender<T> = std::sync::mpsc::SyncSender<T>;

#[inline]
pub(crate) fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    std::sync::mpsc::channel()
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    all(target_os = "freebsd", notify_freebsd_inotify),
    target_os = "windows",
))]
#[inline]
pub(crate) fn bounded<T>(cap: usize) -> (BoundSender<T>, Receiver<T>) {
    std::sync::mpsc::sync_channel(cap)
}

#[cfg(all(target_os = "macos", not(feature = "macos_kqueue")))]
pub use crate::fsevent::FsEventWatcher;
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    all(target_os = "freebsd", notify_freebsd_inotify),
))]
pub use crate::inotify::INotifyWatcher;
#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios",
    all(target_os = "macos", feature = "macos_kqueue")
))]
pub use crate::kqueue::KqueueWatcher;
pub use null::NullWatcher;
pub use poll::PollWatcher;
#[cfg(target_os = "windows")]
pub use windows::ReadDirectoryChangesWatcher;

#[cfg(all(target_os = "macos", not(feature = "macos_kqueue")))]
pub mod fsevent;
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    all(target_os = "freebsd", notify_freebsd_inotify),
))]
pub mod inotify;
#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "ios",
    all(target_os = "macos", feature = "macos_kqueue")
))]
pub mod kqueue;
#[cfg(target_os = "windows")]
pub mod windows;

pub mod null;
pub mod poll;

mod config;
mod error;
mod paths;


/// The set of requirements for watcher event handling functions.
///
/// # Example implementation
///
/// ```text
/// use notify::{Event, Result, EventHandler};
///
/// /// Prints received events
/// struct EventPrinter;
///
/// impl EventHandler for EventPrinter {
///     fn handle_event(&mut self, event: Result<Event>) {
///         if let Ok(event) = event {
///             println!("Event: {:?}", event);
///         }
///     }
/// }
/// ```
pub trait EventHandler: Send + 'static {
    /// Handles an event.
    fn handle_event(&mut self, event: Result<Event>);
}

impl<F> EventHandler for F
where
    F: FnMut(Result<Event>) + Send + 'static,
{
    fn handle_event(&mut self, event: Result<Event>) {
        (self)(event);
    }
}

#[cfg(feature = "crossbeam-channel")]
impl EventHandler for crossbeam_channel::Sender<Result<Event>> {
    fn handle_event(&mut self, event: Result<Event>) {
        let _ = self.send(event);
    }
}

#[cfg(feature = "flume")]
impl EventHandler for flume::Sender<Result<Event>> {
    fn handle_event(&mut self, event: Result<Event>) {
        let _ = self.send(event);
    }
}

#[cfg(feature = "futures")]
impl EventHandler for futures::channel::mpsc::UnboundedSender<Result<Event>> {
    fn handle_event(&mut self, event: Result<Event>) {
        let _ = self.unbounded_send(event);
    }
}

#[cfg(feature = "tokio")]
impl EventHandler for tokio::sync::mpsc::UnboundedSender<Result<Event>> {
    fn handle_event(&mut self, event: Result<Event>) {
        let _ = self.send(event);
    }
}

impl EventHandler for std::sync::mpsc::Sender<Result<Event>> {
    fn handle_event(&mut self, event: Result<Event>) {
        let _ = self.send(event);
    }
}

/// Watcher kind enumeration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WatcherKind {
    /// Inotify backend (Linux, Android, and FreeBSD 14.5+)
    Inotify,
    /// FS-Event backend (mac)
    Fsevent,
    /// KQueue backend (bsd,optionally mac)
    Kqueue,
    /// Polling based backend (fallback)
    PollWatcher,
    /// Windows backend
    ReadDirectoryChangesWatcher,
    /// Fake watcher for testing
    NullWatcher,
}

/// Type that can deliver file activity notifications
///
/// `Watcher` is implemented per platform using the best implementation available on that platform.
/// In addition to such event driven implementations, a polling implementation is also provided
/// that should work on any platform.
///
/// # Creating a watcher
///
/// Because `Watcher` is a trait, Rust usually can't infer the concrete watcher type when calling
/// `Watcher::new(...)` directly. Prefer [`recommended_watcher`] / [`RecommendedWatcher`] (or a
/// specific backend like [`PollWatcher`]) when constructing a watcher.
///
/// ```text
/// use notify::{Event, RecursiveMode, Result, Watcher};
/// use std::{path::Path, sync::mpsc};
///
/// fn main() -> Result<()> {
///     let (tx, _rx) = mpsc::channel::<Result<Event>>();
///     let mut watcher = notify::recommended_watcher(tx)?;
///     watcher.watch(Path::new("."), RecursiveMode::Recursive)?;
///     Ok(())
/// }
/// ```
pub trait Watcher {
    /// Create a new watcher with an initial Config.
    fn new<F: EventHandler>(event_handler: F, config: config::Config) -> Result<Self>
    where
        Self: Sized;
    /// Begin watching a new path.
    ///
    /// If the `path` is a directory, `recursive_mode` will be evaluated. If `recursive_mode` is
    /// `RecursiveMode::Recursive` events will be delivered for all files in that tree. Otherwise
    /// only the directory and its immediate children will be watched.
    ///
    /// If the `path` is a file, `recursive_mode` will be ignored and events will be delivered only
    /// for the file.
    ///
    /// Event paths are reported using the same root representation as `path`. If `path` is
    /// relative, emitted event paths are relative to the process current directory at the time this
    /// method is called. If `path` is absolute, emitted event paths are absolute. Convert `path`
    /// before calling this method if your application needs a specific representation.
    ///
    /// On success, calling this method again for the same backend-resolved path replaces the
    /// existing watch for that path. The recursive mode is updated to the new value, a second
    /// independent watch is not added, and a single call to [`Watcher::unwatch`] removes the
    /// watched path.
    ///
    /// On some platforms, if the `path` is renamed or removed while being watched, behaviour may
    /// be unexpected. See discussions in [#165] and [#166]. If less surprising behaviour is wanted
    /// one may non-recursively watch the _parent_ directory as well and manage related events.
    ///
    /// [#165]: https://github.com/notify-rs/notify/issues/165
    /// [#166]: https://github.com/notify-rs/notify/issues/166
    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()>;

    /// Begin watching a path with per-path settings.
    ///
    /// Behaves like [`Watcher::watch`], with the options in `config` applied to this watch alone.
    /// Backends that do not implement a given option ignore it, so the default implementation
    /// applies only [`WatchPathConfig::recursive_mode`].
    ///
    /// # Errors
    ///
    /// Returns an error in the case that `path` does not exist or if adding the watch fails.
    fn watch_with(&mut self, path: &Path, config: WatchPathConfig) -> Result<()> {
        self.watch(path, config.recursive_mode())
    }

    /// Stop watching a path.
    ///
    /// # Errors
    ///
    /// Returns an error in the case that `path` has not been watched or if removing the watch
    /// fails.
    fn unwatch(&mut self, path: &Path) -> Result<()>;

    /// Add/remove paths to watch in batch.
    ///
    /// For some [`Watcher`] implementations this method provides better performance than multiple
    /// calls to [`Watcher::watch`] and [`Watcher::unwatch`] if you want to add/remove many paths at once.
    ///
    /// # Errors
    ///
    /// Returns [`UpdatePathsError`] if any operation fails. Operations are applied sequentially.
    /// When an error occurs, processing stops: operations before `origin` have been applied,
    /// `origin` is the operation that failed (if known), and `remaining` are the operations that
    /// were not attempted. `remaining` does not include `origin`.
    ///
    /// # Examples
    ///
    /// ```text
    /// # use notify::{Watcher, RecursiveMode, PathOp};
    /// # use std::path::{Path, PathBuf};
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let many_paths_to_add: Vec<PathBuf> = vec![];
    /// # let many_paths_to_remove: Vec<PathBuf> = vec![];
    /// let mut watcher = notify::NullWatcher;
    /// let mut batch = Vec::new();
    ///
    /// for path in many_paths_to_add {
    ///     batch.push(PathOp::watch_recursive(path));
    /// }
    ///
    /// for path in many_paths_to_remove {
    ///     batch.push(PathOp::unwatch(path));
    /// }
    ///
    /// // real work is done there
    /// watcher.update_paths(batch)?;
    /// # Ok(())
    /// # }
    /// ```
    fn update_paths(&mut self, ops: Vec<PathOp>) -> StdResult<(), UpdatePathsError> {
        update_paths(ops, |op| match op {
            PathOp::Watch(path, config) => self
                .watch_with(&path, config.clone())
                .map_err(|e| (PathOp::Watch(path, config), e)),
            PathOp::Unwatch(path) => self.unwatch(&path).map_err(|e| (PathOp::Unwatch(path), e)),
        })
    }

    /// Configure the watcher at runtime.
    ///
    /// See the [`Config`](config/struct.Config.html) struct for all configuration options.
    ///
    /// # Returns
    ///
    /// - `Ok(true)` on success.
    /// - `Ok(false)` if the watcher does not support or implement the option.
    /// - `Err(notify::Error)` on failure.
    fn configure(&mut self, _option: Config) -> Result<bool> {
        Ok(false)
    }

    /// Returns the currently watched paths and their recursive modes.
    ///
    /// Returned paths use the same representation that was passed to [`Watcher::watch`] or
    /// [`Watcher::update_paths`].
    ///
    /// # Errors
    ///
    /// Returns an error if the watcher implementation cannot provide this information.
    fn watched_paths(&self) -> Result<Vec<(PathBuf, RecursiveMode)>> {
        Err(Error::generic(
            "listing watched paths is not supported by this watcher",
        ))
    }

    /// Returns the watcher kind, allowing to perform backend-specific tasks
    fn kind() -> WatcherKind
    where
        Self: Sized;
}

/// The recommended [`Watcher`] implementation for the current platform
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    all(target_os = "freebsd", notify_freebsd_inotify),
))]
pub type RecommendedWatcher = INotifyWatcher;
/// The recommended [`Watcher`] implementation for the current platform
#[cfg(all(target_os = "macos", not(feature = "macos_kqueue")))]
pub type RecommendedWatcher = FsEventWatcher;
/// The recommended [`Watcher`] implementation for the current platform
#[cfg(target_os = "windows")]
pub type RecommendedWatcher = ReadDirectoryChangesWatcher;
/// The recommended [`Watcher`] implementation for the current platform
#[cfg(any(
    all(target_os = "freebsd", not(notify_freebsd_inotify)),
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios",
    all(target_os = "macos", feature = "macos_kqueue")
))]
pub type RecommendedWatcher = KqueueWatcher;
/// The recommended [`Watcher`] implementation for the current platform
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "windows",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios"
)))]
pub type RecommendedWatcher = PollWatcher;

/// Convenience method for creating the [`RecommendedWatcher`] for the current platform.
///
/// This is often the most ergonomic way to construct a watcher, because calling `Watcher::new(...)`
/// requires specifying the concrete watcher type.
///
/// ```text
/// use notify::{RecursiveMode, Result, Watcher};
/// use std::path::Path;
///
/// fn main() -> Result<()> {
///     let mut watcher = notify::recommended_watcher(|res| println!("event: {res:?}"))?;
///     watcher.watch(Path::new("."), RecursiveMode::Recursive)?;
///     Ok(())
/// }
/// ```
pub fn recommended_watcher<F>(event_handler: F) -> Result<RecommendedWatcher>
where
    F: EventHandler,
{
    // All recommended watchers currently implement `new`, so just call that.
    RecommendedWatcher::new(event_handler, Config::default())
}

pub(crate) fn update_paths<F>(ops: Vec<PathOp>, mut apply: F) -> StdResult<(), UpdatePathsError>
where
    F: FnMut(PathOp) -> StdResult<(), (PathOp, Error)>,
{
    let mut iter = ops.into_iter();
    while let Some(op) = iter.next() {
        if let Err((error_op, source)) = apply(op) {
            return Err(UpdatePathsError {
                source,
                origin: Some(error_op),
                remaining: iter.collect(),
            });
        }
    }
    Ok(())
}


