#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Test fixtures and assertions must fail immediately on unexpected setup or filesystem calls"
)]

use bytes::Bytes;
use codex_file_system::*;
use codex_utils_path_uri::PathUri;
use futures::executor::block_on;
use std::collections::VecDeque;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;

#[derive(Default)]
struct TestFileSystem {
    reads: Mutex<VecDeque<Vec<io::Result<Bytes>>>>,
    batches: Mutex<VecDeque<ReadDirectoryOutcome>>,
    limits: Mutex<Vec<(PathUri, usize)>>,
    active: AtomicUsize,
    peak: AtomicUsize,
}

fn root() -> PathUri {
    PathUri::parse("file:///C:/fixture").unwrap()
}

fn options(max_entries: usize) -> WalkOptions {
    WalkOptions {
        max_depth: 4,
        max_directories: 20,
        max_entries,
        follow_directory_symlinks: false,
        prune_hidden_directories: false,
    }
}

fn entry(name: &str) -> ReadDirectoryEntry {
    // Deliberately inaccurate hints: the walker must consult metadata.
    ReadDirectoryEntry {
        file_name: name.into(),
        is_directory: true,
        is_file: false,
    }
}

impl ExecutorFileSystem for TestFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async move { Ok(path.clone()) })
    }
    fn read_file<'a>(
        &'a self,
        _: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        panic!("unbounded file read")
    }
    fn read_file_stream<'a>(
        &'a self,
        _: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async move {
            Ok(FileSystemReadStream::new(futures::stream::iter(
                self.reads
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected read"),
            )))
        })
    }
    fn write_file<'a>(
        &'a self,
        _: &'a PathUri,
        _: Vec<u8>,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected write")
    }
    fn create_directory<'a>(
        &'a self,
        _: &'a PathUri,
        _: CreateDirectoryOptions,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected mkdir")
    }
    fn remove<'a>(
        &'a self,
        _: &'a PathUri,
        _: RemoveOptions,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected remove")
    }
    fn copy<'a>(
        &'a self,
        _: &'a PathUri,
        _: &'a PathUri,
        _: CopyOptions,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected copy")
    }
    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async move {
            if *path != root() {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
                // Make the first sorted file finish after later probes, so an
                // unordered pipeline fails the result-order assertion below.
                let mut pending_polls = if path.to_string().ends_with("/file-00") {
                    3
                } else {
                    1
                };
                futures::future::poll_fn(|cx| {
                    if pending_polls == 0 {
                        Poll::Ready(())
                    } else {
                        pending_polls -= 1;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
            let is_directory = *path == root()
                || path.to_string().ends_with("/dir")
                || path.to_string().ends_with("/.hidden");
            Ok(FileMetadata {
                is_directory,
                is_file: !is_directory,
                is_symlink: false,
                size: 0,
                created_at_ms: 0,
                modified_at_ms: 0,
            })
        })
    }
    fn read_directory<'a>(
        &'a self,
        _: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        panic!("unbounded directory read")
    }
    fn read_directory_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        limit: usize,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome> {
        Box::pin(async move {
            self.limits.lock().unwrap().push((path.clone(), limit));
            self.batches
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "no bounded backend"))
        })
    }
}

fn read_with(
    first: Vec<io::Result<Bytes>>,
    second: Vec<io::Result<Bytes>>,
    limit: usize,
) -> io::Result<Option<Vec<u8>>> {
    let fs = TestFileSystem {
        reads: Mutex::new(VecDeque::from([first, second])),
        ..Default::default()
    };
    block_on(fs.read_file_bounded(&root(), limit, None))
}

#[test]
fn bounded_read_compares_bytes_across_chunk_boundaries() {
    assert_eq!(
        read_with(
            vec![Ok(Bytes::from_static(b"abc"))],
            vec![Ok(Bytes::from_static(b"a")), Ok(Bytes::from_static(b"bc"))],
            3
        )
        .unwrap(),
        Some(b"abc".to_vec())
    );
    assert_eq!(read_with(vec![], vec![], 0).unwrap(), Some(vec![]));
    for second in [b"ab".as_slice(), b"abcd", b"axc"] {
        assert_eq!(
            read_with(
                vec![Ok(Bytes::from_static(b"abc"))],
                vec![Ok(Bytes::copy_from_slice(second))],
                4
            )
            .unwrap(),
            None
        );
    }
}

#[test]
fn bounded_read_preserves_overflow_and_late_error_behavior() {
    assert_eq!(
        read_with(vec![Ok(Bytes::from_static(b"four"))], vec![], 3).unwrap(),
        None
    );
    assert_eq!(
        read_with(
            vec![Ok(Bytes::from_static(b"abc"))],
            vec![Ok(Bytes::from_static(b"abcd"))],
            3
        )
        .unwrap(),
        None
    );
    let error = read_with(
        vec![Ok(Bytes::from_static(b"abc"))],
        vec![
            Ok(Bytes::from_static(b"x")),
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "late error",
            )),
        ],
        3,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "late error");
}

#[test]
fn walk_charges_raw_entries_and_stops_before_exhausted_reads() {
    let fs = TestFileSystem {
        batches: Mutex::new(VecDeque::from([
            ReadDirectoryOutcome {
                entries: vec![entry("dir")],
                entries_examined: 3,
                limit_reached: false,
            },
            ReadDirectoryOutcome {
                entries: vec![entry("dir")],
                entries_examined: 2,
                limit_reached: true,
            },
        ])),
        ..Default::default()
    };
    let outcome = block_on(fs.walk(&root(), options(5), None)).unwrap();
    assert_eq!(
        *fs.limits.lock().unwrap(),
        [(root(), 5), (root().join("dir").unwrap(), 2)]
    );
    assert!(outcome.truncated);
    assert_eq!(outcome.entries.len(), 2);
    assert_eq!(outcome.entries[1].path, root().join("dir/dir").unwrap());
    assert!(outcome.errors.is_empty());
}

#[test]
fn walk_sorts_bounded_batch_and_pipelines_metadata_in_order() {
    let fs = TestFileSystem {
        batches: Mutex::new(VecDeque::from([ReadDirectoryOutcome {
            entries: (0..12)
                .rev()
                .map(|index| entry(&format!("file-{index:02}")))
                .collect(),
            entries_examined: 12,
            limit_reached: false,
        }])),
        ..Default::default()
    };
    let outcome = block_on(fs.walk(&root(), options(20), None)).unwrap();
    assert_eq!(
        outcome
            .entries
            .iter()
            .map(|item| item.path.clone())
            .collect::<Vec<_>>(),
        (0..12)
            .map(|index| root().join(&format!("file-{index:02}")).unwrap())
            .collect::<Vec<_>>()
    );
    assert!(
        outcome
            .entries
            .iter()
            .all(|item| item.kind == WalkEntryKind::File)
    );
    assert_eq!(fs.peak.load(Ordering::SeqCst), 8);
    assert_eq!(fs.active.load(Ordering::SeqCst), 0);
    assert!(!outcome.truncated);
    assert!(outcome.errors.is_empty());
}

#[test]
fn walk_reports_depth_truncation_without_reading_beyond_limit() {
    let fs = TestFileSystem {
        batches: Mutex::new(VecDeque::from([ReadDirectoryOutcome {
            entries: vec![entry("dir"), entry("file")],
            entries_examined: 2,
            limit_reached: false,
        }])),
        ..Default::default()
    };
    let mut walk_options = options(20);
    walk_options.max_depth = 0;
    let outcome = block_on(fs.walk(&root(), walk_options, None)).unwrap();

    assert!(outcome.truncated);
    assert!(outcome.errors.is_empty());
    assert_eq!(
        outcome.entries,
        vec![
            WalkEntry {
                path: root().join("dir").unwrap(),
                kind: WalkEntryKind::Directory,
            },
            WalkEntry {
                path: root().join("file").unwrap(),
                kind: WalkEntryKind::File,
            },
        ]
    );
    assert_eq!(*fs.limits.lock().unwrap(), [(root(), 20)]);
}

#[test]
fn walk_at_depth_limit_is_complete_when_no_eligible_directories_remain() {
    for name in ["file", ".hidden"] {
        let fs = TestFileSystem {
            batches: Mutex::new(VecDeque::from([ReadDirectoryOutcome {
                entries: vec![entry(name)],
                entries_examined: 1,
                limit_reached: false,
            }])),
            ..Default::default()
        };
        let mut walk_options = options(20);
        walk_options.max_depth = 0;
        walk_options.prune_hidden_directories = true;
        let outcome = block_on(fs.walk(&root(), walk_options, None)).unwrap();

        assert!(!outcome.truncated, "unexpected truncation for {name}");
        assert!(outcome.errors.is_empty());
        assert_eq!(outcome.entries.len(), 1);
        assert_eq!(outcome.entries[0].path, root().join(name).unwrap());
        assert_eq!(
            outcome.entries[0].kind,
            if name == ".hidden" {
                WalkEntryKind::Directory
            } else {
                WalkEntryKind::File
            }
        );
        assert_eq!(*fs.limits.lock().unwrap(), [(root(), 20)]);
    }
}

#[test]
fn walk_reports_missing_bounded_backend() {
    let fs = TestFileSystem::default();
    assert_eq!(
        block_on(fs.walk(&root(), options(2), None))
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
}

#[test]
fn walk_preserves_producer_truncation_without_returned_entries() {
    let fs = TestFileSystem {
        batches: Mutex::new(VecDeque::from([ReadDirectoryOutcome {
            entries: vec![],
            entries_examined: 2,
            limit_reached: true,
        }])),
        ..Default::default()
    };
    let outcome = block_on(fs.walk(&root(), options(2), None)).unwrap();
    assert!(outcome.truncated);
    assert!(outcome.entries.is_empty());
    assert!(outcome.errors.is_empty());
    assert_eq!(*fs.limits.lock().unwrap(), [(root(), 2)]);
}
