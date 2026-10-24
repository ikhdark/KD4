#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::time::Instant;

#[derive(Default)]
struct Reporter {
    terminal: Mutex<Vec<FileSearchSnapshot>>,
    ready: Condvar,
    cancel_on_progress: Option<Arc<AtomicBool>>,
}

impl SessionReporter for Reporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        if !snapshot.match_complete
            && !snapshot.cancelled
            && let Some(cancel) = &self.cancel_on_progress
        {
            cancel.store(true, Ordering::Relaxed);
        }
    }
    fn on_complete(&self, _: &str) {}
    fn on_complete_snapshot(&self, snapshot: &FileSearchSnapshot) {
        self.terminal.lock().unwrap().push(snapshot.clone());
        self.ready.notify_all();
    }
}

impl Reporter {
    fn wait(&self, count: usize) -> FileSearchSnapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut snapshots = self.terminal.lock().unwrap();
        while snapshots.len() < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "query did not finish");
            snapshots = self.ready.wait_timeout(snapshots, remaining).unwrap().0;
        }
        snapshots[count - 1].clone()
    }
}

#[test]
fn empty_root_still_reports_ancestor_ignore_parse_errors() {
    let parent = tempfile::tempdir().unwrap();
    fs::write(parent.path().join(".ignore"), "[z-a]\n").unwrap();
    let root = parent.path().join("empty");
    fs::create_dir(&root).unwrap();
    let result = run("absent", vec![root], FileSearchOptions::default(), None).unwrap();
    assert!(!result.walk_complete);
    assert!(result.matches.is_empty());
}

#[test]
fn reused_syntax_changing_query_finds_the_same_literal_as_a_fresh_search() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("foo$bar.txt"), "target").unwrap();
    let reporter = Arc::new(Reporter::default());
    let session = create_session(
        vec![root.path().into()],
        FileSearchOptions::default(),
        reporter.clone(),
        None,
    )
    .unwrap();
    session.update_query("foo$");
    assert!(reporter.wait(1).matches.is_empty());
    session.update_query("foo$bar");
    let reused = reporter.wait(2);
    let fresh = run(
        "foo$bar",
        vec![root.path().into()],
        FileSearchOptions::default(),
        None,
    )
    .unwrap();
    assert!(reused.match_complete && reused.walk_complete && !reused.cancelled);
    assert_eq!(
        reused.matches.iter().map(|m| &m.path).collect::<Vec<_>>(),
        [Path::new("foo$bar.txt")]
    );
    assert_eq!(reused.matches, fresh.matches);
    assert_eq!(
        reused.scanned_file_count, 2,
        "query reuse must retain the original index"
    );
    assert!(!can_filter_previous_candidates("foo$", "foo$bar"));
    assert!(!can_filter_previous_candidates("foo\\", "foo\\$"));
    assert!(can_filter_previous_candidates("src/con", "src/config.rs"));
}

#[test]
fn exact_basename_survives_every_small_cutoff_ahead_of_suffix_noise() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("z/path")).unwrap();
    fs::write(root.path().join("z/path/config.rs"), "target").unwrap();
    for index in (0..80).rev() {
        fs::write(root.path().join(format!("config.rs.{index:02}")), "noise").unwrap();
    }
    for limit in [1, 5, 20, 50, 64] {
        let result = run(
            "config.rs",
            vec![root.path().into()],
            FileSearchOptions {
                limit: NonZero::new(limit).unwrap(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert!(result.walk_complete);
        assert_eq!(result.total_match_count, 81);
        assert_eq!(result.matches[0].path, Path::new("z/path/config.rs"));
    }
}

#[test]
fn qualified_excludes_have_the_same_scope_in_each_root_and_root_order() {
    let home = tempfile::tempdir().unwrap();
    let roots = [home.path().join("a"), home.path().join("b")];
    for root in &roots {
        fs::create_dir_all(root.join("generated")).unwrap();
        fs::write(root.join("needle.rs"), "keep").unwrap();
        fs::write(root.join("generated/needle.rs"), "exclude").unwrap();
    }
    for exclude in ["generated/**", "/generated/**"] {
        for ordered in [roots.to_vec(), roots.iter().rev().cloned().collect()] {
            let result = run(
                "needle",
                ordered,
                FileSearchOptions {
                    exclude: vec![exclude.into()],
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            assert!(result.walk_complete);
            assert_eq!(
                result
                    .matches
                    .iter()
                    .map(FileMatch::full_path)
                    .collect::<Vec<_>>(),
                roots
                    .iter()
                    .map(|root| root.join("needle.rs"))
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn cancellation_publishes_terminal_data_before_completion() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("needle.rs"), "target").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let reporter = Arc::new(Reporter {
        cancel_on_progress: Some(cancel.clone()),
        ..Default::default()
    });
    let session = create_session(
        vec![root.path().into()],
        FileSearchOptions::default(),
        reporter.clone(),
        Some(cancel),
    )
    .unwrap();
    session.update_query("needle");
    let terminal = reporter.wait(1);
    assert_eq!(terminal.query, "needle");
    assert!(terminal.cancelled);
    assert!(!terminal.match_complete);
    drop(session);
}

#[test]
fn ignored_entries_cannot_hide_raw_work_after_cancellation() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join(".ignore"), "*.ignored\n").unwrap();
    for index in 0..100 {
        fs::write(root.path().join(format!("{index}.ignored")), "").unwrap();
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let advances = Arc::new(AtomicUsize::new(0));
    let observer_count = advances.clone();
    let observer_cancel = cancel.clone();
    let (tx, rx) = unbounded();
    let nucleo = Nucleo::new(Config::DEFAULT.match_paths(), Arc::new(|| {}), Some(1), 1);
    let inner = Arc::new(SessionInner {
        search_directories: vec![root.path().into()],
        limit: 20,
        compute_indices: false,
        respect_gitignore: true,
        walk_limits: FILE_SEARCH_WALK_LIMITS,
        cancelled: cancel,
        shutdown: Arc::new(AtomicBool::new(false)),
        reporter: Arc::new(Reporter::default()),
        work_tx: tx,
        latest_query: LatestQuery::default(),
        raw_walk_observer: Some(Arc::new(move || {
            if observer_count.fetch_add(1, Ordering::Relaxed) == 2 {
                observer_cancel.store(true, Ordering::Relaxed);
            }
        })),
    });
    walker_worker(inner, vec![(root.path().into(), None)], nucleo.injector());
    assert_eq!(
        advances.load(Ordering::Relaxed),
        3,
        "retire at the next raw boundary, not after the ignored tail"
    );
    assert!(matches!(
        rx.recv().unwrap(),
        WorkSignal::WalkComplete { complete: false }
    ));
    let uncancelled = run(
        ".ignore",
        vec![root.path().into()],
        FileSearchOptions::default(),
        None,
    )
    .unwrap();
    assert!(uncancelled.walk_complete);
    assert_eq!(uncancelled.scanned_file_count, 2);
}
#[test]
fn exact_unicode_spelling_is_not_replaced_by_the_matchers_grapheme_projection() {
    let root = tempfile::tempdir().unwrap();
    for name in ["👩.rs", "👩‍💻.rs", "cafe.rs", "café.rs"] {
        fs::write(root.path().join(name), "").unwrap();
    }
    for query in ["👩‍💻.rs", "café.rs"] {
        let result = run(
            query,
            vec![root.path().into()],
            FileSearchOptions {
                limit: NonZero::new(1).unwrap(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(result.matches[0].path, Path::new(query));
    }
}
#[test]
fn escaped_space_and_dollar_edits_rematch_the_retained_index() {
    for (query, target) in [(r"foo\ bar", "foo bar.txt"), (r"foo\$", "foo$.txt")] {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(target), "target").unwrap();
        let reporter = Arc::new(Reporter::default());
        let session = create_session(
            vec![root.path().into()],
            FileSearchOptions::default(),
            reporter.clone(),
            None,
        )
        .unwrap();
        session.update_query("foo\\");
        reporter.wait(1);
        session.update_query(query);
        let reused = reporter.wait(2);
        let fresh = run(
            query,
            vec![root.path().into()],
            FileSearchOptions::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            reused
                .matches
                .iter()
                .map(|m| m.path.as_path())
                .collect::<Vec<_>>(),
            [Path::new(target)]
        );
        assert_eq!(reused.matches, fresh.matches);
        assert!(reused.match_complete && reused.walk_complete);
    }
}
