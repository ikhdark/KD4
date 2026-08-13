use super::*;

#[tokio::test]
async fn multi_selector_recovery_returns_several_omitted_sections_exactly() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text("primary\ncaller\ntest\n").with_sections(vec![
        ToolProjectionSection {
            id: "primary".to_string(),
            value: None,
            exact_bytes: 8,
            inclusion: ToolProjectionInclusion::Omitted,
            canonical_range: Some(CanonicalByteRange::new(0, 8)),
            children: Vec::new(),
            recovery_chunk_bytes: None,
        },
        ToolProjectionSection {
            id: "caller".to_string(),
            value: None,
            exact_bytes: 7,
            inclusion: ToolProjectionInclusion::Omitted,
            canonical_range: Some(CanonicalByteRange::new(8, 15)),
            children: Vec::new(),
            recovery_chunk_bytes: None,
        },
        ToolProjectionSection {
            id: "test".to_string(),
            value: None,
            exact_bytes: 5,
            inclusion: ToolProjectionInclusion::Omitted,
            canonical_range: Some(CanonicalByteRange::new(15, 20)),
            children: Vec::new(),
            recovery_chunk_bytes: None,
        },
    ]);
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");

    let recovered = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        ["primary", "caller", "test"]
            .into_iter()
            .map(|id| ToolOutputSelector::Section { id: id.to_string() })
            .collect(),
    )
    .await
    .expect("recover omitted sections");

    assert_eq!(
        recovered
            .results
            .iter()
            .map(|result| (
                result.status,
                result.complete,
                result.text.as_deref(),
                result.continuation.as_ref(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (ToolOutputSelectorStatus::Ok, true, Some("primary\n"), None),
            (ToolOutputSelectorStatus::Ok, true, Some("caller\n"), None),
            (ToolOutputSelectorStatus::Ok, true, Some("test\n"), None),
        ]
    );
}

#[derive(Debug, Eq, PartialEq)]
struct ProjectionMeasurement {
    initial_model_tokens: usize,
    recovery_calls: u32,
    recovery_generations: u32,
    canonical_bytes: usize,
    artifact_reads: u32,
    recovery_retruncations: u32,
    strict_subset_rereads: u32,
    match_index_complete: bool,
}

#[tokio::test]
async fn four_chunk_fixture_recovers_every_omitted_chunk_in_one_combined_call() {
    let temp = tempfile::tempdir().expect("tempdir");
    let content = (1..=126)
        .map(|line| format!("line {line:03} {}\r\n", "x".repeat(80)))
        .collect::<String>();
    let line_ranges = content
        .split_inclusive('\n')
        .scan(0_u64, |cursor, line| {
            let start = *cursor;
            *cursor += line.len() as u64;
            Some((start, *cursor))
        })
        .collect::<Vec<_>>();
    let mut sections = Vec::new();
    for (index, lines) in [(1, 40), (41, 80), (81, 120), (121, 126)]
        .into_iter()
        .enumerate()
    {
        let (start_line, end_line) = lines;
        let start = line_ranges[start_line - 1].0;
        let end = line_ranges[end_line - 1].1;
        sections.push(ToolProjectionSection {
            id: format!("src:fixture:L{start_line}-L{end_line}"),
            value: None,
            exact_bytes: end - start,
            inclusion: if index < 2 {
                ToolProjectionInclusion::Included
            } else {
                ToolProjectionInclusion::Omitted
            },
            canonical_range: Some(CanonicalByteRange::new(start, end)),
            children: Vec::new(),
            recovery_chunk_bytes: None,
        });
    }
    let canonical = CanonicalToolResult::text(content.clone()).with_sections(sections.clone());
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");
    let omitted = sections
        .iter()
        .filter(|section| section.inclusion == ToolProjectionInclusion::Omitted)
        .map(|section| ToolOutputSelector::Section {
            id: section.id.clone(),
        })
        .collect::<Vec<_>>();

    let recovered = read_tool_output_selectors(temp.path(), "thread", &artifact_id, omitted)
        .await
        .expect("recover all omitted source chunks");
    assert!(
        recovered
            .results
            .iter()
            .all(|result| { result.status == ToolOutputSelectorStatus::Ok && result.complete })
    );
    let recovered_text = recovered
        .results
        .iter()
        .map(|result| result.text.as_deref().expect("UTF-8 source chunk"))
        .collect::<String>();
    let omitted_start = sections[2].canonical_range.expect("range").start as usize;
    assert_eq!(recovered_text, content[omitted_start..]);

    let legacy = ProjectionMeasurement {
        initial_model_tokens: approx_token_count(&content[..8 * 1024]),
        recovery_calls: 2,
        recovery_generations: 2,
        canonical_bytes: content.len(),
        artifact_reads: 0,
        recovery_retruncations: 0,
        strict_subset_rereads: 2,
        match_index_complete: true,
    };
    let projected = ProjectionMeasurement {
        initial_model_tokens: approx_token_count(
            &content[..sections[1].canonical_range.expect("range").end as usize],
        ),
        recovery_calls: 1,
        recovery_generations: 1,
        canonical_bytes: canonical.exact_bytes as usize,
        artifact_reads: 1,
        recovery_retruncations: 0,
        strict_subset_rereads: 0,
        match_index_complete: true,
    };
    assert_eq!(projected.canonical_bytes, legacy.canonical_bytes);
    assert!(projected.recovery_calls < legacy.recovery_calls);
    assert!(projected.recovery_generations < legacy.recovery_generations);
    assert!(projected.strict_subset_rereads < legacy.strict_subset_rereads);
    assert_eq!(projected.recovery_retruncations, 0);
    assert!(projected.match_index_complete);
}

#[tokio::test]
async fn aggregate_recovery_reserves_space_for_later_ranges_and_returns_continuations() {
    let temp = tempfile::tempdir().expect("tempdir");
    let line = "x".repeat(16_000);
    let canonical = CanonicalToolResult::text(format!("{line}\n{line}\n{line}\n"));
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");
    let selectors = (1..=3)
        .map(|line| ToolOutputSelector::Lines {
            start: line,
            end: line,
        })
        .collect::<Vec<_>>();

    let recovered =
        read_tool_output_selectors(temp.path(), "thread", &artifact_id, selectors.clone())
            .await
            .expect("recover fair aggregate");

    assert_eq!(
        recovered
            .results
            .iter()
            .map(|result| result.status)
            .collect::<Vec<_>>(),
        vec![
            ToolOutputSelectorStatus::Ok,
            ToolOutputSelectorStatus::Ok,
            ToolOutputSelectorStatus::AggregateOmitted,
        ],
        "complete selectors must be admitted in request order",
    );
    let omitted = &recovered.results[2];
    assert!(!omitted.complete);
    assert_eq!(omitted.continuation.as_ref(), Some(&omitted.selector));
    assert!(response_fits_recovery_ceiling(&recovered));
}

#[tokio::test]
async fn byte_selectors_return_utf8_directly_and_non_utf8_as_base64() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::bytes(vec![b'h', b'i', 0xff, 0x00]);
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");

    let recovered = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        vec![
            ToolOutputSelector::Bytes { start: 0, end: 2 },
            ToolOutputSelector::Bytes { start: 2, end: 4 },
        ],
    )
    .await
    .expect("recover byte ranges");

    assert_eq!(recovered.results[0].text.as_deref(), Some("hi"));
    assert_eq!(recovered.results[0].data_base64, None);
    assert_eq!(recovered.results[1].text, None);
    assert_eq!(
        BASE64_STANDARD
            .decode(
                recovered.results[1]
                    .data_base64
                    .as_deref()
                    .expect("base64 bytes"),
            )
            .expect("valid base64"),
        vec![0xff, 0x00],
    );
}

#[tokio::test]
async fn oversized_json_pointer_exposes_exact_chunkable_canonical_range() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::json(serde_json::json!({
        "huge": "x".repeat(80_000),
        "small": "retained",
    }));
    let expected_range = canonical.json_pointers["/huge"].range;
    let expected =
        canonical.bytes[expected_range.start as usize..expected_range.end as usize].to_vec();
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");

    let oversized = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        vec![ToolOutputSelector::JsonPointer {
            pointer: "/huge".to_string(),
        }],
    )
    .await
    .expect("select oversized pointer");
    assert!(response_fits_recovery_ceiling(&oversized));
    let selected = &oversized.results[0];
    assert_eq!(selected.status, ToolOutputSelectorStatus::SelectorTooLarge);
    assert_eq!(selected.exact_bytes, Some(expected_range.len()));
    assert_eq!(selected.canonical_range, Some(expected_range));
    let plan = selected
        .subdivision_plan
        .as_ref()
        .expect("bounded byte-subdivision plan");
    assert!(plan.chunk_bytes > 0);

    let selectors = (expected_range.start..expected_range.end)
        .step_by(plan.chunk_bytes as usize)
        .map(|start| ToolOutputSelector::Bytes {
            start,
            end: start
                .saturating_add(plan.chunk_bytes)
                .min(expected_range.end),
        })
        .collect::<Vec<_>>();
    let mut recovered = Vec::new();
    for selector in selectors {
        let chunk = read_tool_output_selectors(temp.path(), "thread", &artifact_id, vec![selector])
            .await
            .expect("recover exact byte chunk");
        assert!(response_fits_recovery_ceiling(&chunk));
        let chunk = &chunk.results[0];
        assert_eq!(chunk.status, ToolOutputSelectorStatus::Ok);
        assert!(chunk.complete);
        if let Some(text) = &chunk.text {
            recovered.extend_from_slice(text.as_bytes());
        } else {
            recovered.extend_from_slice(
                &BASE64_STANDARD
                    .decode(chunk.data_base64.as_deref().expect("base64 bytes"))
                    .expect("valid base64"),
            );
        }
    }
    assert_eq!(recovered, expected);

    let artifact_files = std::fs::read_dir(temp.path().join("tool-output/thread"))
        .expect("artifact directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(
        artifact_files
            .iter()
            .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
            .count(),
        1,
        "recovery must not recursively spill into another artifact",
    );
}

#[tokio::test]
async fn active_output_file_lock_blocks_removal_until_release() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("active.log");
    tokio::fs::write(&path, b"active")
        .await
        .expect("write active artifact");
    let active = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open active artifact");
    active.try_lock().expect("lock active artifact");

    assert!(matches!(
        remove_inactive_output_path(path.clone()).await,
        InactiveRemovalOutcome::Active
    ));
    assert!(path.exists());

    drop(active);
    assert!(matches!(
        remove_inactive_output_path(path.clone()).await,
        InactiveRemovalOutcome::RemovedOrAbsent
    ));
    assert!(!path.exists());
}

#[tokio::test]
async fn replacement_does_not_truncate_before_acquiring_the_lock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"retained output").await;
    let RawOutputArtifact::Stored { path, .. } = &artifact else {
        panic!("expected stored artifact");
    };
    let active = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open active artifact");
    active.try_lock().expect("lock active artifact");

    let replaced = replace_raw_output_artifact(&artifact, b"replacement").await;

    assert!(matches!(replaced, RawOutputArtifact::Failed { .. }));
    drop(active);
    assert_eq!(
        tokio::fs::read(path).await.expect("read retained artifact"),
        b"retained output"
    );
}

#[tokio::test]
async fn per_thread_retention_skips_active_artifacts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let directory = temp.path().join("tool-output").join("thread");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("artifact directory");
    let active_path = directory.join("0000.log");
    tokio::fs::write(&active_path, b"active")
        .await
        .expect("active artifact");
    let active = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&active_path)
        .expect("open active artifact");
    active.try_lock().expect("lock active artifact");
    for index in 1..=(max_retained_artifacts_per_thread() + 2) {
        tokio::fs::write(directory.join(format!("{index:04}.log")), b"inactive")
            .await
            .expect("inactive artifact");
    }
    let keep_path = directory.join(format!(
        "{:04}.log",
        max_retained_artifacts_per_thread() + 2
    ));

    enforce_retention(&directory, &keep_path).await;

    assert!(active_path.exists());
    assert!(keep_path.exists());
    let mut entries = tokio::fs::read_dir(&directory)
        .await
        .expect("read artifact directory");
    let mut count = 0;
    while entries
        .next_entry()
        .await
        .expect("read artifact entry")
        .is_some()
    {
        count += 1;
    }
    assert_eq!(count, max_retained_artifacts_per_thread());
    drop(active);
}

#[tokio::test]
async fn global_retention_bounds_artifacts_across_threads() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("tool-output");
    let total = max_retained_artifacts_total() + 5;
    for index in 0..total {
        let directory = root.join(format!("thread-{}", index % 4));
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("thread directory");
        tokio::fs::write(directory.join(format!("{index:04}.log")), b"artifact")
            .await
            .expect("artifact");
    }
    let keep_path = root.join("thread-0").join("keep.log");
    tokio::fs::write(&keep_path, b"keep")
        .await
        .expect("keep artifact");

    enforce_global_retention(&root, &keep_path).await;

    let mut retained = 0;
    let mut thread_directories = tokio::fs::read_dir(&root).await.expect("tool output root");
    while let Some(thread) = thread_directories
        .next_entry()
        .await
        .expect("thread directory")
    {
        let mut entries = tokio::fs::read_dir(thread.path())
            .await
            .expect("thread artifacts");
        while entries
            .next_entry()
            .await
            .expect("artifact entry")
            .is_some()
        {
            retained += 1;
        }
    }
    assert_eq!(retained, max_retained_artifacts_total());
    assert!(keep_path.exists());
}

#[tokio::test]
async fn protected_evidence_artifact_survives_per_thread_retention_without_reducing_generic_limit()
{
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_evidence_output_artifact(temp.path(), "thread", b"durable evidence")
        .await
        .expect("pending evidence")
        .mark_durable();
    let RawOutputArtifact::Stored { path, .. } = &artifact else {
        panic!("expected stored artifact");
    };

    for index in 0..(max_retained_artifacts_per_thread() + 5) {
        create_raw_output_artifact(temp.path(), "thread", format!("generic-{index}").as_bytes())
            .await;
    }

    assert!(path.exists());
    assert!(evidence_protection_path(path).exists());
    let mut generic_logs = 0;
    let mut total_logs = 0;
    let mut entries = tokio::fs::read_dir(path.parent().expect("thread directory"))
        .await
        .expect("thread artifacts");
    while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
        if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
            total_logs += 1;
            if entry.path() != *path {
                generic_logs += 1;
            }
        }
    }
    assert_eq!(generic_logs, max_retained_artifacts_per_thread());
    assert_eq!(total_logs, max_retained_artifacts_per_thread() + 1);
}

#[tokio::test]
async fn evidence_creation_holds_retention_permit_until_marker_is_durable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let evidence_barrier = Arc::clone(&barrier);
    let codex_home = temp.path().to_path_buf();
    let evidence_task = tokio::spawn(async move {
        create_evidence_output_artifact_inner(
            &codex_home,
            "thread",
            b"durable evidence",
            Some(evidence_barrier.as_ref()),
        )
        .await
    });

    barrier.wait().await;
    let directory = temp.path().join("tool-output").join("thread");
    let mut entries = tokio::fs::read_dir(&directory)
        .await
        .expect("thread artifacts");
    let log_path = loop {
        let entry = entries
            .next_entry()
            .await
            .expect("artifact entry")
            .expect("evidence log");
        if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
            break entry.path();
        }
    };
    assert_eq!(
        tokio::fs::symlink_metadata(&log_path)
            .await
            .expect("synced evidence log")
            .len(),
        b"durable evidence".len() as u64
    );
    assert!(!evidence_protection_path(&log_path).exists());

    let mut churn = tokio::task::JoinSet::new();
    for index in 0..(max_retained_artifacts_per_thread() + 1) {
        let codex_home = temp.path().to_path_buf();
        churn.spawn(async move {
            create_raw_output_artifact(&codex_home, "thread", format!("generic-{index}").as_bytes())
                .await
        });
    }

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let mut log_count = 0;
            let mut entries = tokio::fs::read_dir(&directory)
                .await
                .expect("thread artifacts");
            while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
                if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
                    log_count += 1;
                }
            }
            if log_count == max_retained_artifacts_per_thread() + 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("generic artifacts should reach the retention gate");

    barrier.wait().await;
    let artifact = evidence_task
        .await
        .expect("evidence creation task")
        .expect("pending evidence")
        .mark_durable();
    while let Some(result) = churn.join_next().await {
        result.expect("generic artifact task");
    }

    let RawOutputArtifact::Stored { path, .. } = artifact else {
        panic!("expected protected evidence artifact");
    };
    assert!(path.exists());
    assert!(evidence_protection_path(&path).exists());
}

#[tokio::test]
async fn active_reader_trim_unprotects_artifact_before_eventual_retention_cleanup() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_evidence_output_artifact(temp.path(), "thread", b"durable evidence")
        .await
        .expect("pending evidence")
        .mark_durable();
    let RawOutputArtifact::Stored { id, path, .. } = &artifact else {
        panic!("expected stored evidence");
    };
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open evidence reader");
    reader.try_lock().expect("lock evidence reader");

    assert!(
        delete_evidence_artifact(temp.path(), "thread", &id.to_string())
            .await
            .is_err()
    );
    assert!(path.exists());
    assert!(!evidence_protection_path(path).exists());
    drop(reader);

    for index in 0..(max_retained_artifacts_per_thread() + 5) {
        create_raw_output_artifact(temp.path(), "thread", format!("generic-{index}").as_bytes())
            .await;
    }
    assert!(!path.exists());
}

#[tokio::test]
async fn cancelled_evidence_creation_leaves_no_protected_orphan() {
    let temp = tempfile::tempdir().expect("tempdir");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let creation_barrier = Arc::clone(&barrier);
    let codex_home = temp.path().to_path_buf();
    let creation_task = tokio::spawn(async move {
        create_evidence_output_artifact_inner(
            &codex_home,
            "thread",
            b"durable evidence",
            Some(creation_barrier.as_ref()),
        )
        .await
    });
    barrier.wait().await;
    creation_task.abort();
    assert!(matches!(
        creation_task.await,
        Err(err) if err.is_cancelled()
    ));

    let directory = temp.path().join("tool-output").join("thread");
    let mut entries = tokio::fs::read_dir(directory)
        .await
        .expect("thread artifacts");
    while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
        let path = entry.path();
        let extension = path.extension().and_then(|value| value.to_str());
        assert_ne!(extension, Some("log"));
        assert_ne!(extension, Some(EVIDENCE_PROTECTION_EXTENSION));
    }
}

#[tokio::test]
async fn cancelled_pending_evidence_lease_cleans_up_but_durable_lease_survives() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pending = create_evidence_output_artifact(temp.path(), "thread", b"pending evidence")
        .await
        .expect("pending evidence");
    let pending_path = pending.path.clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let persistence_barrier = Arc::clone(&barrier);
    let persistence_task = tokio::spawn(async move {
        persistence_barrier.wait().await;
        persistence_barrier.wait().await;
        pending.mark_durable()
    });
    barrier.wait().await;
    persistence_task.abort();
    assert!(
        persistence_task
            .await
            .expect_err("persistence should be cancelled")
            .is_cancelled()
    );
    assert!(!pending_path.exists());
    assert!(!evidence_protection_path(&pending_path).exists());

    let durable = create_evidence_output_artifact(temp.path(), "thread", b"durable evidence")
        .await
        .expect("pending durable evidence")
        .mark_durable();
    let RawOutputArtifact::Stored { path, .. } = durable else {
        panic!("expected durable evidence");
    };
    assert!(path.exists());
    assert!(evidence_protection_path(&path).exists());
}

#[tokio::test]
async fn global_retention_skips_protected_evidence_without_broadening_generic_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact =
        create_evidence_output_artifact(temp.path(), "evidence-thread", b"durable evidence")
            .await
            .expect("pending evidence")
            .mark_durable();
    let RawOutputArtifact::Stored {
        path: evidence_path,
        ..
    } = &artifact
    else {
        panic!("expected stored artifact");
    };
    let root = temp.path().join("tool-output");
    for index in 0..(max_retained_artifacts_total() + 5) {
        let directory = root.join(format!("generic-thread-{}", index % 16));
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("thread directory");
        tokio::fs::write(directory.join(format!("{index:04}.log")), b"generic")
            .await
            .expect("generic artifact");
    }
    let keep_path = root.join("generic-thread-0").join("keep.log");
    tokio::fs::write(&keep_path, b"keep")
        .await
        .expect("keep artifact");

    assert_eq!(
        force_retention_reconciliation_for_test(&root).await,
        RetentionModeKind::Indexed
    );
    enforce_global_retention(&root, &keep_path).await;

    assert!(evidence_path.exists());
    let mut generic_logs = 0;
    let mut directories = tokio::fs::read_dir(&root).await.expect("tool output root");
    while let Some(directory) = directories.next_entry().await.expect("thread directory") {
        let mut entries = tokio::fs::read_dir(directory.path())
            .await
            .expect("thread artifacts");
        while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
            if entry.path().extension().and_then(|value| value.to_str()) == Some("log")
                && entry.path() != *evidence_path
            {
                generic_logs += 1;
            }
        }
    }
    assert_eq!(generic_logs, max_retained_artifacts_total());
}

#[tokio::test]
async fn retention_sweeps_are_serialized() {
    let temp = tempfile::tempdir().expect("tempdir");
    let directory = temp.path().join("tool-output").join("thread");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("artifact directory");
    let keep_path = directory.join("keep.log");
    tokio::fs::write(&keep_path, b"keep")
        .await
        .expect("keep artifact");
    let retention_permit = retention_sweep_permit().await;
    let mut sweep = tokio::spawn({
        let directory = directory.clone();
        let keep_path = keep_path.clone();
        async move { enforce_retention(&directory, &keep_path).await }
    });

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut sweep)
            .await
            .is_err(),
        "a concurrent sweep must wait for the process-wide retention lock"
    );
    drop(retention_permit);
    tokio::time::timeout(std::time::Duration::from_secs(1), &mut sweep)
        .await
        .expect("retention sweep should resume after lock release")
        .expect("retention sweep task");
}

async fn create_artifacts_and_measure(count: usize) -> (RetentionDiagnostics, std::time::Duration) {
    let temp = tempfile::tempdir().expect("tempdir");
    let started = Instant::now();
    for index in 0..count {
        let artifact = create_raw_output_artifact(
            temp.path(),
            "thread",
            format!("artifact-{index}").as_bytes(),
        )
        .await;
        assert!(matches!(artifact, RawOutputArtifact::Stored { .. }));
    }
    let elapsed = started.elapsed();
    let root = temp.path().join("tool-output");
    (retention_diagnostics_for_test(&root), elapsed)
}

#[tokio::test]
async fn indexed_retention_scans_only_at_configured_boundaries() {
    let (at_100, wall_100) = create_artifacts_and_measure(100).await;
    let (at_127, wall_127) = create_artifacts_and_measure(127).await;
    let (at_131, wall_131) = create_artifacts_and_measure(131).await;

    assert_eq!(at_100.scans, 1, "100 artifacts took {wall_100:?}");
    assert_eq!(at_127.scans, 2, "127 artifacts took {wall_127:?}");
    assert_eq!(at_131.scans, 2, "131 artifacts took {wall_131:?}");
    assert_eq!(at_100.logical_mutations, 100);
    assert_eq!(at_127.logical_mutations, 127);
    assert_eq!(at_131.logical_mutations, 131);
    assert_eq!(at_131.evictions, 3);
}

#[tokio::test]
async fn streaming_chunks_update_bytes_but_count_as_one_logical_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"").await;
    let state = Arc::new(Mutex::new(artifact));
    let root = temp.path().join("tool-output");
    let before = retention_diagnostics_for_test(&root);
    let mut writer = RawOutputArtifactWriter::open(Some(&state))
        .await
        .expect("streaming writer");

    for _ in 0..200 {
        writer.write_chunk(Some(&state), b"x").await;
    }
    writer.finish(Some(&state)).await;

    let after = retention_diagnostics_for_test(&root);
    assert_eq!(
        after.streaming_size_updates - before.streaming_size_updates,
        201
    );
    assert_eq!(after.logical_mutations - before.logical_mutations, 1);
    assert_eq!(after.scans, before.scans);
}

#[tokio::test]
async fn near_limit_streaming_growth_reconciles_before_the_next_retention_decision() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"").await;
    let RawOutputArtifact::Stored { path, .. } = &artifact else {
        panic!("expected stored artifact");
    };
    let sparse = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open sparse artifact");
    sparse
        .set_len(MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD - RETENTION_BYTE_GUARD_BAND - 1)
        .expect("extend sparse artifact");
    drop(sparse);
    let state = Arc::new(Mutex::new(artifact));
    let mut writer = RawOutputArtifactWriter::open(Some(&state))
        .await
        .expect("streaming writer");
    writer.write_chunk(Some(&state), b"xx").await;
    writer.finish(Some(&state)).await;

    let root = temp.path().join("tool-output");
    let before = retention_diagnostics_for_test(&root);
    let artifact_path = {
        let artifact = state.lock().await;
        let RawOutputArtifact::Stored { path, .. } = &*artifact else {
            panic!("expected stored artifact");
        };
        path.clone()
    };
    enforce_retention(
        artifact_path.parent().expect("artifact directory"),
        &artifact_path,
    )
    .await;

    let after = retention_diagnostics_for_test(&root);
    assert_eq!(after.reconciliations, before.reconciliations + 1);
}

#[tokio::test]
async fn stale_streaming_writer_cannot_update_a_rebuilt_generation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"").await;
    let state = Arc::new(Mutex::new(artifact));
    let root = temp.path().join("tool-output");
    let mut writer = RawOutputArtifactWriter::open(Some(&state))
        .await
        .expect("streaming writer");
    let writer_generation = writer
        .retention_token
        .as_ref()
        .and_then(|token| token.generation)
        .expect("writer generation");

    assert_eq!(
        force_retention_reconciliation_for_test(&root).await,
        RetentionModeKind::Indexed
    );
    assert_ne!(
        retention_generation_for_test(&root),
        Some(writer_generation)
    );
    writer.write_chunk(Some(&state), b"late").await;

    let dirty_generation = retention_generation_for_test(&root).expect("dirty generation");
    assert_ne!(dirty_generation, writer_generation);
    assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Dirty);
    let diagnostics = retention_diagnostics_for_test(&root);
    assert_eq!(diagnostics.stale_delta_rejections, 1);
    assert_eq!(diagnostics.dirty_transitions, 1);

    writer.finish(Some(&state)).await;
    let path = {
        let artifact = state.lock().await;
        let RawOutputArtifact::Stored { path, .. } = &*artifact else {
            panic!("expected stored artifact");
        };
        path.clone()
    };
    enforce_retention(path.parent().expect("artifact directory"), &path).await;
    assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Indexed);
}

#[tokio::test]
async fn every_indexed_mutation_publisher_rejects_a_stale_generation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"artifact").await;
    let RawOutputArtifact::Stored { path, .. } = &artifact else {
        panic!("expected stored artifact");
    };
    let root = temp.path().join("tool-output");

    for mutation in [
        LogicalRetentionMutation::Create,
        LogicalRetentionMutation::AppendReplace,
        LogicalRetentionMutation::Protection,
        LogicalRetentionMutation::EvidenceReconcile,
    ] {
        let stale = capture_retention_token(path.parent().expect("artifact directory"));
        assert_eq!(
            force_retention_reconciliation_for_test(&root).await,
            RetentionModeKind::Indexed
        );
        let record = artifact_retention_record(path)
            .await
            .expect("artifact metadata")
            .expect("artifact record");
        publish_known_record(&stale, record, mutation);
        assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Dirty);
        assert_eq!(
            force_retention_reconciliation_for_test(&root).await,
            RetentionModeKind::Indexed
        );
    }

    let stale = capture_retention_token(path.parent().expect("artifact directory"));
    assert_eq!(
        force_retention_reconciliation_for_test(&root).await,
        RetentionModeKind::Indexed
    );
    publish_known_remove(&stale, path, LogicalRetentionMutation::Delete, false);
    assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Dirty);
    assert_eq!(
        retention_diagnostics_for_test(&root).stale_delta_rejections,
        5
    );
}

#[tokio::test]
async fn reconciliation_releases_registry_mutex_and_rejects_known_internal_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"").await;
    let state = Arc::new(Mutex::new(artifact));
    let mut writer = RawOutputArtifactWriter::open(Some(&state))
        .await
        .expect("streaming writer");
    let root = temp.path().join("tool-output");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    set_reconciliation_barrier(&root, Arc::clone(&barrier));
    let reconcile_root = root.clone();
    let reconciliation =
        tokio::spawn(async move { force_retention_reconciliation_for_test(&reconcile_root).await });

    barrier.wait().await;
    assert!(retention_registry_mutex_is_available_for_test());
    writer.write_chunk(Some(&state), b"concurrent").await;
    barrier.wait().await;

    assert_eq!(
        reconciliation.await.expect("reconciliation task"),
        RetentionModeKind::Dirty
    );
    assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Dirty);
    writer.finish(Some(&state)).await;
}

#[tokio::test]
async fn token_captured_during_reconciliation_cannot_update_the_installed_candidate() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"artifact").await;
    let RawOutputArtifact::Stored { path, .. } = &artifact else {
        panic!("expected stored artifact");
    };
    let root = temp.path().join("tool-output");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    set_reconciliation_barrier(&root, Arc::clone(&barrier));
    let reconcile_root = root.clone();
    let reconciliation =
        tokio::spawn(async move { force_retention_reconciliation_for_test(&reconcile_root).await });

    barrier.wait().await;
    let token = capture_retention_token(path.parent().expect("artifact directory"));
    assert_eq!(token.starting_mode, RetentionModeKind::Reconciling);
    let reconciling_generation = token.generation.expect("reconciling generation");
    barrier.wait().await;
    assert_eq!(
        reconciliation.await.expect("reconciliation task"),
        RetentionModeKind::Indexed
    );
    assert_eq!(
        retention_generation_for_test(&root),
        Some(reconciling_generation)
    );

    let record = artifact_retention_record(path)
        .await
        .expect("artifact metadata")
        .expect("artifact record");
    publish_known_record(&token, record, LogicalRetentionMutation::AppendReplace);

    assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Dirty);
    assert_ne!(
        retention_generation_for_test(&root),
        Some(reconciling_generation)
    );
}

#[tokio::test]
async fn detectable_external_inconsistency_discards_the_reconciliation_candidate() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"artifact").await;
    drop(artifact);
    let root = temp.path().join("tool-output");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    set_reconciliation_barrier(&root, Arc::clone(&barrier));
    let reconcile_root = root.clone();
    let reconciliation =
        tokio::spawn(async move { force_retention_reconciliation_for_test(&reconcile_root).await });

    barrier.wait().await;
    tokio::fs::remove_dir_all(&root)
        .await
        .expect("remove root during reconciliation");
    barrier.wait().await;

    assert_eq!(
        reconciliation.await.expect("reconciliation task"),
        RetentionModeKind::Dirty
    );
    assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Dirty);
}

#[tokio::test]
async fn generation_is_not_reused_after_root_eviction_and_reinitialization() {
    let temp = tempfile::tempdir().expect("tempdir");
    let roots = (0..=MAX_RETENTION_INDEX_ROOTS)
        .map(|index| temp.path().join(format!("tool-output-{index}")))
        .collect::<Vec<_>>();
    let mut registry = RetentionRegistry::default();
    let first = insert_dirty_root(
        &mut registry,
        roots[0].clone(),
        RetentionDiagnostics::default(),
    )
    .expect("first generation");
    for root in &roots[1..MAX_RETENTION_INDEX_ROOTS] {
        insert_dirty_root(&mut registry, root.clone(), RetentionDiagnostics::default())
            .expect("root generation");
    }
    assert_eq!(registry.roots.len(), MAX_RETENTION_INDEX_ROOTS);

    insert_dirty_root(
        &mut registry,
        roots[MAX_RETENTION_INDEX_ROOTS].clone(),
        RetentionDiagnostics::default(),
    )
    .expect("evicting generation");
    assert!(!registry.roots.contains_key(&roots[0]));
    let second = insert_dirty_root(
        &mut registry,
        roots[0].clone(),
        RetentionDiagnostics::default(),
    )
    .expect("reinitialized generation");

    assert_ne!(first, second);
    assert_eq!(MAX_RETENTION_INDEX_ROOTS, 4);
}

#[tokio::test]
async fn invalid_protection_marker_fails_reconciliation_open() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"artifact").await;
    let RawOutputArtifact::Stored { path, .. } = artifact else {
        panic!("expected stored artifact");
    };
    std::fs::write(evidence_protection_path(&path), b"invalid marker")
        .expect("write invalid marker");
    let root = temp.path().join("tool-output");

    assert_eq!(
        force_retention_reconciliation_for_test(&root).await,
        RetentionModeKind::Dirty
    );
    assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Dirty);
}

#[tokio::test]
async fn periodic_scan_only_reconciliation_exits_after_capacity_recovers() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("tool-output");
    let directory = root.join("thread");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("artifact directory");
    std::fs::write(directory.join("00000.log"), b"x").expect("first artifact");
    std::fs::write(directory.join("00001.log"), b"x").expect("second artifact");
    set_retention_index_capacity_for_test(&root, 1);
    assert_eq!(
        force_retention_reconciliation_for_test(&root).await,
        RetentionModeKind::ScanOnly
    );
    std::fs::remove_file(directory.join("00001.log")).expect("shrink root");

    for _ in 0..RETENTION_RECONCILIATION_INTERVAL - 1 {
        assert_eq!(
            prepare_retention_mode(&root, false).await,
            RetentionModeKind::ScanOnly
        );
    }
    assert_eq!(
        prepare_retention_mode(&root, false).await,
        RetentionModeKind::Indexed
    );
    assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Indexed);
    assert_eq!(retention_diagnostics_for_test(&root).scan_only_exits, 1);
}

#[tokio::test]
async fn stale_generation_invalidates_a_rebuilt_scan_only_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("tool-output");
    let directory = root.join("thread");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("artifact directory");
    std::fs::write(directory.join("00000.log"), b"x").expect("first artifact");
    std::fs::write(directory.join("00001.log"), b"x").expect("second artifact");
    set_retention_index_capacity_for_test(&root, 1);
    assert_eq!(
        force_retention_reconciliation_for_test(&root).await,
        RetentionModeKind::ScanOnly
    );
    let stale = capture_retention_token(&directory);
    let stale_generation = stale.generation.expect("scan-only generation");
    assert_eq!(stale.starting_mode, RetentionModeKind::ScanOnly);
    assert_eq!(
        force_retention_reconciliation_for_test(&root).await,
        RetentionModeKind::ScanOnly
    );
    assert_ne!(retention_generation_for_test(&root), Some(stale_generation));

    publish_known_remove(
        &stale,
        &directory.join("absent.log"),
        LogicalRetentionMutation::Delete,
        false,
    );

    assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Dirty);
}

#[tokio::test]
async fn oversized_root_is_sticky_scan_only_until_an_authoritative_in_capacity_scan() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("tool-output");
    let directory = root.join("thread");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("artifact directory");
    for index in 0..=MAX_RETENTION_INDEX_RECORDS {
        std::fs::write(directory.join(format!("{index:05}.log")), b"x")
            .expect("write indexed artifact");
    }
    let _ = capture_retention_token(&directory);

    assert_eq!(
        force_retention_reconciliation_for_test(&root).await,
        RetentionModeKind::ScanOnly
    );
    let entered = retention_diagnostics_for_test(&root);
    assert_eq!(entered.oversized_root_fallbacks, 1);
    assert_eq!(entered.scan_only_entries, 1);
    assert_eq!(entered.candidates_visited, 8_193);
    for _ in 0..5 {
        assert_eq!(
            prepare_retention_mode(&root, false).await,
            RetentionModeKind::ScanOnly
        );
    }
    let sticky = retention_diagnostics_for_test(&root);
    assert_eq!(sticky.reconciliations, entered.reconciliations);
    assert_eq!(
        sticky.scan_only_operations,
        entered.scan_only_operations + 5
    );

    std::fs::remove_file(directory.join("08192.log")).expect("shrink oversized root");
    assert_eq!(
        force_retention_reconciliation_for_test(&root).await,
        RetentionModeKind::Indexed
    );
    let exited = retention_diagnostics_for_test(&root);
    assert_eq!(exited.scan_only_exits, 1);
    assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Indexed);
}
