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
    metadata_batches: Mutex<VecDeque<ReadDirectoryOutcome<WalkDirectoryEntry>>>,
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
        filters: Default::default(),
    }
}

#[test]
fn filtered_walk_preserves_requested_file_coverage_and_prunes_only_explicit_directories() {
    let fs = TestFileSystem {
        batches: Mutex::new(VecDeque::from([
            ReadDirectoryOutcome {
                entries: ["dir", ".hidden", "a.rs", "b.rs", "a.txt", "λ.rs"].into_iter().map(entry).collect(),
                entries_examined: 6, limit_reached: false,
            },
            ReadDirectoryOutcome { entries: vec![entry("c.rs")], entries_examined: 1, limit_reached: false },
        ])), ..Default::default()
    };
    let mut opts = options(20);
    opts.filters = WalkFilters {
        include: vec!["?.rs".into()], exclude: vec!["b*".into()], exclude_directories: vec!["dir".into()],
    };
    let outcome = block_on(fs.walk(&root(), opts.clone(), None)).unwrap();
    assert_eq!(outcome.applied_filters, Some(opts.filters));
    assert!(!outcome.truncated);
    assert!(outcome.errors.is_empty());
    assert_eq!(outcome.entries.iter().map(|entry| entry.path.clone()).collect::<Vec<_>>(),
        [".hidden", "a.rs", "λ.rs", ".hidden/c.rs"].map(|name| root().join(name).unwrap()));
    assert_eq!(fs.limits.lock().unwrap().iter().map(|(path, _)| path.clone()).collect::<Vec<_>>(),
        [root(), root().join(".hidden").unwrap()]);
}

#[test]
fn filters_do_not_hide_budget_cutoffs_or_exclude_the_selected_root() {
    for limited in [false, true] {
        let fs = TestFileSystem {
            batches: Mutex::new(VecDeque::from([ReadDirectoryOutcome {
                entries: vec![entry("other.txt")], entries_examined: 1, limit_reached: limited,
            }])), ..Default::default()
        };
        let mut opts = options(2);
        opts.filters.include = vec!["*.rs".into()];
        opts.filters.exclude_directories = vec!["fixture".into()];
        let outcome = block_on(fs.walk(&root(), opts, None)).unwrap();
        assert!(outcome.entries.is_empty());
        assert_eq!(outcome.truncated, limited);
        assert_eq!(fs.limits.lock().unwrap().len(), 1);
        if limited { assert_eq!(outcome.unexplored[0].reason, "entry_limit"); }
    }
}

#[test]
fn filters_are_bounded_and_invalid_scope_fails_before_enumeration() {
    for field in ["include", "exclude", "exclude_directories"] {
        for patterns in [vec!["".into()], vec!["src/*.rs".into()], vec!["a\\b".into()],
            vec!["a\0b".into()], vec!["x".repeat(257)], vec!["x".into(); 65]] {
            let fs = TestFileSystem::default();
            let mut opts = options(20);
            match field {
                "include" => opts.filters.include = patterns,
                "exclude" => opts.filters.exclude = patterns,
                _ => opts.filters.exclude_directories = patterns,
            }
            assert_eq!(block_on(fs.walk(&root(), opts, None)).unwrap_err().kind(), io::ErrorKind::InvalidInput, "{field}");
            assert!(fs.limits.lock().unwrap().is_empty(), "{field}");
        }
    }
}

#[test]
fn filter_pattern_budget_is_shared_across_fields_and_counts_utf8_bytes() {
    let mut filters = WalkFilters {
        include: vec!["*.rs".into(); 32],
        exclude: vec!["test_*".into(); 31],
        exclude_directories: vec!["target".into()],
    };
    filters.validate().expect("64 total patterns are permitted");
    filters.exclude_directories.push("vendor".into());
    assert_eq!(filters.validate().unwrap_err().kind(), io::ErrorKind::InvalidInput);

    filters = WalkFilters {
        include: vec!["界".repeat(85)],
        ..Default::default()
    };
    filters.validate().expect("255 UTF-8 bytes are permitted");
    filters.include[0].push('界');
    assert_eq!(filters.validate().unwrap_err().kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn response_byte_cutoff_reports_its_effective_bound() {
    let fs = TestFileSystem {
        batches: Mutex::new(VecDeque::from([ReadDirectoryOutcome {
            entries: vec![entry(&"x".repeat(MAX_WALK_RESPONSE_BYTES))], entries_examined: 1, limit_reached: false,
        }])), ..Default::default()
    };
    let outcome = block_on(fs.walk(&root(), options(20), None)).unwrap();
    assert!(outcome.truncated);
    assert!(outcome.entries.is_empty());
    assert_eq!(outcome.unexplored[0].reason, "response_bytes");
    assert_eq!(outcome.unexplored[0].effective_limit, MAX_WALK_RESPONSE_BYTES);
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

    fn read_directory_bounded_for_walk<'a>(
        &'a self,
        path: &'a PathUri,
        limit: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome<WalkDirectoryEntry>> {
        Box::pin(async move {
            if let Some(batch) = self.metadata_batches.lock().unwrap().pop_front() {
                self.limits.lock().unwrap().push((path.clone(), limit));
                return Ok(batch);
            }
            // Most backends inherit the trait's default adapter; run that one over
            // `batches` instead of restating it here.
            let adapter = DefaultWalkAdapter(self);
            adapter
                .read_directory_bounded_for_walk(path, limit, sandbox)
                .await
        })
    }
}

/// View of the fixture that does not override `read_directory_bounded_for_walk`.
struct DefaultWalkAdapter<'fs>(&'fs TestFileSystem);

impl ExecutorFileSystem for DefaultWalkAdapter<'_> {
    fn canonicalize<'a>(
        &'a self,
        _: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        panic!("unexpected canonicalize through the default walk adapter")
    }
    fn read_file<'a>(
        &'a self,
        _: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        panic!("unexpected file read through the default walk adapter")
    }
    fn read_file_stream<'a>(
        &'a self,
        _: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        panic!("unexpected stream read through the default walk adapter")
    }
    fn write_file<'a>(
        &'a self,
        _: &'a PathUri,
        _: Vec<u8>,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected write through the default walk adapter")
    }
    fn create_directory<'a>(
        &'a self,
        _: &'a PathUri,
        _: CreateDirectoryOptions,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected mkdir through the default walk adapter")
    }
    fn remove<'a>(
        &'a self,
        _: &'a PathUri,
        _: RemoveOptions,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected remove through the default walk adapter")
    }
    fn copy<'a>(
        &'a self,
        _: &'a PathUri,
        _: &'a PathUri,
        _: CopyOptions,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected copy through the default walk adapter")
    }
    fn get_metadata<'a>(
        &'a self,
        _: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        panic!("unexpected metadata probe through the default walk adapter")
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
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome> {
        self.0.read_directory_bounded(path, limit, sandbox)
    }
}

#[test]
fn walk_reuses_enumerated_metadata_without_followup_probes() {
    assert!(std::mem::size_of::<WalkDirectoryEntry>() <= std::mem::size_of::<ReadDirectoryEntry>());
    let metadata = WalkEntryMetadata {
        is_directory: false,
        is_file: true,
        is_symlink: false,
    };
    let fs = TestFileSystem {
        metadata_batches: Mutex::new(VecDeque::from([ReadDirectoryOutcome {
            entries: ["b", "a"]
                .into_iter()
                .map(|name| WalkDirectoryEntry {
                    file_name: name.into(),
                    metadata: Some(metadata),
                })
                .collect(),
            entries_examined: 3,
            limit_reached: false,
        }])),
        ..Default::default()
    };
    let outcome = block_on(fs.walk(&root(), options(5), None)).unwrap();
    assert_eq!(
        outcome
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>(),
        [root().join("a").unwrap(), root().join("b").unwrap()]
    );
    assert_eq!(fs.peak.load(Ordering::SeqCst), 0);
    assert_eq!(*fs.limits.lock().unwrap(), [(root(), 5)]);
    assert!(outcome.errors.is_empty());
    assert!(!outcome.truncated);
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
    assert_eq!(outcome.unexplored, vec![WalkStop {
        path: root().join("dir").unwrap(),
        reason: "depth_limit".into(),
        effective_limit: 0,
        partially_examined: false,
    }]);
}

#[test]
fn walk_directory_budget_includes_root_and_preserves_unvisited_frontier() {
    let fs = TestFileSystem {
        batches: Mutex::new(VecDeque::from([ReadDirectoryOutcome {
            entries: vec![entry("dir"), entry("file")],
            entries_examined: 2,
            limit_reached: false,
        }])),
        ..Default::default()
    };
    let mut opts = options(20);
    opts.max_directories = 1;
    let outcome = block_on(fs.walk(&root(), opts, None)).unwrap();
    assert!(outcome.truncated);
    assert!(outcome.errors.is_empty());
    assert_eq!(outcome.entries, vec![
        WalkEntry { path: root().join("dir").unwrap(), kind: WalkEntryKind::Directory },
        WalkEntry { path: root().join("file").unwrap(), kind: WalkEntryKind::File },
    ]);
    assert_eq!(outcome.unexplored, vec![WalkStop {
        path: root().join("dir").unwrap(),
        reason: "directory_limit".into(),
        effective_limit: 1,
        partially_examined: false,
    }]);
    assert_eq!(*fs.limits.lock().unwrap(), [(root(), 20)]);
}

#[test]
fn confined_read_default_fails_closed_without_opening_streams() {
    let fs = TestFileSystem::default();
    let path = root().join("file").unwrap();
    let error = block_on(fs.read_file_bounded_confined(&path, &root(), 10, None))
        .expect_err("a backend without confinement must not use the unconfined fallback");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
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
    assert_eq!(outcome.unexplored, vec![WalkStop { path:root(), reason:"entry_limit".into(),
        effective_limit:2, partially_examined:true }]);
}

#[test]
fn walk_frontier_is_bounded_without_losing_unexplored_scope() {
    let fs = TestFileSystem {
        metadata_batches: Mutex::new(VecDeque::from([ReadDirectoryOutcome {
            entries: (0..100).map(|index| WalkDirectoryEntry { file_name:format!("dir-{index:03}"),
                metadata:Some(WalkEntryMetadata { is_directory:true, is_file:false, is_symlink:false }) }).collect(),
            entries_examined:100, limit_reached:false,
        }])), ..Default::default()
    };
    let mut limits = options(200);
    limits.max_depth = 0;
    let outcome = block_on(fs.walk(&root(), limits, None)).unwrap();
    assert_eq!(outcome.entries.len(), 100);
    assert_eq!(outcome.unexplored.len(), 64);
    assert_eq!(outcome.unexplored[0].reason, "depth_limit");
    assert_eq!(outcome.unexplored[0].effective_limit, 0);
    assert!(!outcome.unexplored[0].partially_examined);
    let fallback = outcome.unexplored.last().unwrap();
    assert_eq!(fallback.path, root());
    assert_eq!(fallback.reason, "frontier_limit");
    assert!(fallback.partially_examined);
    assert_eq!(*fs.limits.lock().unwrap(), [(root(), 200)]);
}
