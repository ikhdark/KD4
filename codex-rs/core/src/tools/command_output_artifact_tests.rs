use super::*;
use std::time::Duration;

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(command_output_artifact)]
async fn confirmed_performance_artifact_filesystem_operations_use_blocking_pool() {
    for (operation_kind, semaphore_registry) in [
        ("create", false),
        ("create", true),
        ("attach", false),
        ("attach", true),
        ("raw_create", false),
        ("raw_create", true),
        ("raw_append", false),
        ("raw_append", true),
        ("raw_replace", false),
        ("raw_replace", true),
        ("raw_stream", false),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let canonical = CanonicalToolResult::text("retained canonical output\n");
        let initial_output = match operation_kind {
            "attach" => Some(canonical.bytes.as_slice()),
            "raw_append" | "raw_stream" => Some(b"retained ".as_slice()),
            "raw_replace" => Some(b"obsolete output\n".as_slice()),
            _ => None,
        };
        let existing = match initial_output {
            Some(output) => Some(create_raw_output_artifact(temp.path(), "thread", output).await),
            None => None,
        };
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_released = Arc::clone(&released);
        let blocker = std::thread::spawn(move || {
            let wait_for_release = || {
                locked_tx.send(()).expect("notify registry held");
                // A watchdog also releases an implementation that blocks the sole
                // runtime thread, so the regression fails instead of hanging.
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                thread_released.store(true, Ordering::Release);
            };
            if semaphore_registry {
                let _guard = RETENTION_SWEEP_SEMAPHORES
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                wait_for_release();
            } else {
                let _guard = lock_retention_registry();
                wait_for_release();
            }
        });
        locked_rx.recv().expect("registry held");
        let home = temp.path().to_path_buf();
        let expected = canonical.bytes.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let operation = tokio::spawn(async move {
            entered_tx.send(()).expect("notify operation entry");
            if operation_kind == "create" || operation_kind == "attach" {
                let artifact = match existing.as_ref().and_then(RawOutputArtifact::artifact_id) {
                    Some(id) => {
                        attach_canonical_output_artifact(
                            &home,
                            "thread",
                            &id.to_string(),
                            &canonical,
                        )
                        .await
                    }
                    None => create_canonical_output_artifact(&home, "thread", &canonical).await,
                };
                assert!(artifact.complete, "{artifact:?}");
                return artifact.artifact_id().expect("canonical artifact ID");
            }
            let artifact = match operation_kind {
                "raw_create" => create_raw_output_artifact(&home, "thread", &canonical.bytes).await,
                "raw_append" => {
                    append_raw_output_artifact(
                        existing.as_ref().expect("existing raw artifact"),
                        b"canonical output\n",
                    )
                    .await
                }
                "raw_replace" => {
                    replace_raw_output_artifact(
                        existing.as_ref().expect("existing raw artifact"),
                        &canonical.bytes,
                    )
                    .await
                }
                "raw_stream" => {
                    let state = Arc::new(Mutex::new(existing.expect("existing raw artifact")));
                    let mut writer = RawOutputArtifactWriter::open(Some(&state))
                        .await
                        .expect("streaming writer");
                    writer
                        .write_chunk(Some(&state), b"canonical output\n")
                        .await;
                    writer.finish(Some(&state)).await;
                    state.lock().await.clone()
                }
                _ => unreachable!("unknown artifact operation"),
            };
            assert!(
                matches!(&artifact, RawOutputArtifact::Stored { .. }),
                "{artifact:?}"
            );
            artifact.artifact_id().expect("raw artifact ID").to_string()
        });
        entered_rx.await.expect("operation entered");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let executor_advanced_while_locked = !released.load(Ordering::Acquire);
        let operation_waited_for_lock = !operation.is_finished();
        let _ = release_tx.send(());
        let artifact_id = operation.await.expect("artifact operation");
        blocker.join().expect("registry blocker");

        assert!(
            executor_advanced_while_locked,
            "operation={operation_kind}, semaphore_registry={semaphore_registry}"
        );
        assert!(
            operation_waited_for_lock,
            "operation={operation_kind}, semaphore_registry={semaphore_registry}"
        );
        let recovered = read_tool_output_selectors(
            temp.path(),
            "thread",
            &artifact_id,
            vec![ToolOutputSelector::Bytes {
                start: 0,
                end: expected.len() as u64,
            }],
        )
        .await
        .expect("read canonical output");
        assert!(recovered.complete);
        assert_eq!(
            recovered.results[0].text.as_deref(),
            Some("retained canonical output\n")
        );
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(command_output_artifact)]
async fn cancelled_canonical_creation_finishes_owned_family_and_releases_retention() {
    let temp = tempfile::tempdir().expect("tempdir");
    let directory = temp.path().join("tool-output/thread");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("artifact directory");
    let semaphore = retention_sweep_semaphore(directory.parent().expect("output root"));
    let permit = semaphore.acquire_owned().await.expect("hold retention");
    let home = temp.path().to_path_buf();
    let operation = tokio::spawn(async move {
        create_canonical_output_artifact(
            &home,
            "thread",
            &CanonicalToolResult::text("cancellation preserves exact bytes\n"),
        )
        .await
    });
    let artifact_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut entries = tokio::fs::read_dir(&directory)
                .await
                .expect("staged directory");
            while let Some(entry) = entries.next_entry().await.expect("staged entry") {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(staged) = name.strip_prefix('.')
                    && let Some((id, _)) = staged.split_once(".segment-")
                {
                    return id.to_string();
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("creation reaches retained-family admission");
    operation.abort();
    assert!(operation.await.expect_err("cancel caller").is_cancelled());
    drop(permit);

    let recovered = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(recovered) = read_tool_output_selectors(
                temp.path(),
                "thread",
                &artifact_id,
                vec![ToolOutputSelector::Lines { start: 1, end: 1 }],
            )
            .await
                && recovered.complete
            {
                break recovered;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("owned operation commits despite caller cancellation");
    assert_eq!(
        recovered.results[0].text.as_deref(),
        Some("cancellation preserves exact bytes\n")
    );
    let permit = tokio::time::timeout(
        Duration::from_secs(5),
        retention_sweep_permit_for_directory(&directory),
    )
    .await
    .expect("worker releases retention")
    .expect("retention permit");
    let mut entries = tokio::fs::read_dir(&directory)
        .await
        .expect("committed directory");
    while let Some(entry) = entries.next_entry().await.expect("committed entry") {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(!name.ends_with(".pending"), "orphan staging: {name}");
        assert!(
            !name.ends_with(".transaction"),
            "unfinished transaction: {name}"
        );
    }
    drop(permit);
}

#[test]
fn artifact_metadata_discloses_source_capture_truncation() {
    let artifact = RawOutputArtifact::unavailable("test artifact");
    let rendered = artifact.render_for_model_with_source_truncation(true);

    assert!(rendered.contains("source capture truncated"));
    assert!(rendered.contains("omitted bytes unavailable"));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn token_efficiency_reduced_output_notice_defers_to_tool_schema() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"retained output").await;

    let notice = artifact
        .reduction_notice()
        .await
        .expect("stored artifact reduction notice");

    assert!(notice.contains("with read_tool_output"));
    assert!(!notice.contains(&artifact.artifact_id().expect("artifact ID").to_string()));
    assert!(notice.contains("do not rerun the producer"));
    assert!(!notice.contains(r#""selectors""#));
    assert!(!notice.contains(r#"{"artifact_id""#));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn multi_selector_recovery_returns_several_omitted_sections_exactly() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut canonical = CanonicalToolResult::text("primary\ncaller\ntest\n");
    canonical.sections = vec![
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
    ];
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

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn exact_recovery_expires_when_the_artifact_is_deleted() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text("one\ntwo\nthree\n");
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");
    let selectors = vec![ToolOutputSelector::Lines { start: 2, end: 3 }];

    read_tool_output_selectors(temp.path(), "thread", &artifact_id, selectors.clone())
        .await
        .expect("first exact recovery");
    std::fs::remove_file(
        temp.path()
            .join("tool-output/thread")
            .join(format!("{artifact_id}.log")),
    )
    .expect("remove artifact after its exact result is proved");

    let error =
        read_tool_output_selectors_with_reuse(temp.path(), "thread", &artifact_id, selectors)
            .await
            .expect_err("deleted artifact must invalidate exact recovery");

    assert_eq!(error, ReadToolOutputError::Expired);
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
#[serial_test::serial(command_output_artifact)]
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
    let mut canonical = CanonicalToolResult::text(content.clone());
    canonical.sections = sections.clone();
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
#[serial_test::serial(command_output_artifact)]
async fn aggregate_recovery_reserves_space_for_later_ranges_and_returns_continuations() {
    let temp = tempfile::tempdir().expect("tempdir");
    let line = "x".repeat(16_000);
    let canonical =
        CanonicalToolResult::text(format!("{line}\nskip one\n{line}\nskip two\n{line}\n"));
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");
    let selectors = [1, 3, 5]
        .into_iter()
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
        "complete selectors must be admitted in canonical source order",
    );
    assert!(!recovered.complete);
    let omitted = &recovered.results[2];
    assert!(!omitted.complete);
    assert!(omitted.canonical_range.is_some());
    assert!(omitted.exact_bytes.is_some());
    assert!(omitted.subdivision_plan.is_some());
    assert!(!omitted.child_selectors.is_empty());
    assert_eq!(
        omitted.continuation.as_ref(),
        omitted.child_selectors.first(),
        "aggregate overflow must advertise the first deterministic byte child",
    );
    assert!(response_fits_recovery_ceiling(&recovered));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn over_truncation_marginal_aggregate_overage_returns_exact_results_without_retry() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text("alpha recovery\nbeta recovery\n");
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");
    let selectors = vec![
        ToolOutputSelector::Lines { start: 1, end: 1 },
        ToolOutputSelector::Lines { start: 2, end: 2 },
    ];
    let full = read_tool_output_selectors_with_ceiling(
        temp.path(),
        "thread",
        &artifact_id,
        selectors.clone(),
        usize::MAX,
    )
    .await
    .expect("measure complete response");
    let full_tokens =
        approx_token_count(&serde_json::to_string(&full).expect("serialize response"));
    let marginal_ceiling = full_tokens.saturating_sub(1);

    let recovered = read_tool_output_selectors_with_ceiling(
        temp.path(),
        "thread",
        &artifact_id,
        selectors,
        marginal_ceiling,
    )
    .await
    .expect("admit a marginal overage instead of forcing another read");

    assert!(!response_fits_recovery_token_ceiling(
        &recovered,
        marginal_ceiling
    ));
    assert!(response_fits_recovery_retry_avoidance_ceiling(
        &recovered,
        marginal_ceiling
    ));
    assert!(recovered.complete);
    assert!(
        recovered
            .results
            .iter()
            .all(|result| { result.status == ToolOutputSelectorStatus::Ok && result.complete })
    );
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn exact_three_kib_section_is_complete_in_one_transaction() {
    let temp = tempfile::tempdir().expect("tempdir");
    let text = (0..48)
        .map(|index| format!("section-{index:03}-{}\n", "x".repeat(51)))
        .collect::<String>();
    assert!((3_000..=3_300).contains(&text.len()));
    let mut canonical = CanonicalToolResult::text(text.clone());
    canonical.sections = vec![ToolProjectionSection {
        id: "three-kib".to_string(),
        value: None,
        exact_bytes: text.len() as u64,
        inclusion: ToolProjectionInclusion::Omitted,
        canonical_range: Some(CanonicalByteRange::new(0, text.len() as u64)),
        children: Vec::new(),
        recovery_chunk_bytes: None,
    }];
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");

    let recovered = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        vec![ToolOutputSelector::Section {
            id: "three-kib".to_string(),
        }],
    )
    .await
    .expect("recover three KiB section");

    assert!(recovered.complete);
    assert_eq!(recovered.results.len(), 1);
    assert_eq!(recovered.results[0].status, ToolOutputSelectorStatus::Ok);
    assert_eq!(recovered.results[0].text.as_deref(), Some(text.as_str()));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn exact_eight_kib_lines_drain_all_subdivisions_when_final_transaction_fits() {
    let temp = tempfile::tempdir().expect("tempdir");
    let text = (0..128)
        .map(|index| format!("line-{index:03}-{}\n", "abcdefghij".repeat(5)))
        .collect::<String>();
    assert!((7_500..=9_000).contains(&text.len()));
    let line_count = text.lines().count();
    let canonical = CanonicalToolResult::text(text.clone());
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");

    let recovered = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        vec![ToolOutputSelector::Lines {
            start: 1,
            end: line_count,
        }],
    )
    .await
    .expect("recover eight KiB line selection");

    assert!(recovered.complete);
    let selected = &recovered.results[0];
    assert_eq!(selected.status, ToolOutputSelectorStatus::Ok);
    assert_eq!(selected.text.as_deref(), Some(text.as_str()));
    assert!(selected.subdivision_plan.is_some());
    assert!(
        selected
            .message
            .as_deref()
            .is_some_and(|message| { message.contains("internally drained all") })
    );
    assert!(selected.continuation.is_none());
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn exact_ranges_are_deduplicated_coalesced_and_returned_in_source_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    let text = "0123456789".repeat(8);
    let canonical = CanonicalToolResult::text(text.clone());
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");

    let recovered = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        vec![
            ToolOutputSelector::Bytes { start: 40, end: 50 },
            ToolOutputSelector::Bytes { start: 20, end: 30 },
            ToolOutputSelector::Bytes { start: 0, end: 10 },
            ToolOutputSelector::Bytes { start: 8, end: 20 },
            ToolOutputSelector::Bytes { start: 20, end: 30 },
        ],
    )
    .await
    .expect("recover normalized exact ranges");

    assert!(recovered.complete);
    assert_eq!(recovered.results.len(), 2);
    assert_eq!(
        recovered
            .results
            .iter()
            .map(|result| result.canonical_range)
            .collect::<Vec<_>>(),
        vec![
            Some(CanonicalByteRange::new(0, 30)),
            Some(CanonicalByteRange::new(40, 50)),
        ]
    );
    assert_eq!(recovered.results[0].text.as_deref(), Some(&text[0..30]));
    assert_eq!(recovered.results[1].text.as_deref(), Some(&text[40..50]));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn byte_selectors_return_utf8_directly_and_non_utf8_as_base64() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::bytes(vec![b'h', b'i', b' ', 0xff, 0x00]);
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");

    let recovered = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        vec![
            ToolOutputSelector::Bytes { start: 0, end: 2 },
            ToolOutputSelector::Bytes { start: 3, end: 5 },
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
#[serial_test::serial(command_output_artifact)]
async fn artifact_recovery_search_returns_batched_exact_selectors_and_continuation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text(
        "header\nneedle alpha\nmiddle\nneedle beta\nmore\nneedle gamma\ntail\n",
    );
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");
    let search = ToolOutputSelector::Search {
        query: "needle".to_string(),
        start_byte: 0,
        max_results: 2,
        context_lines: 1,
    };

    let indexed = read_tool_output_selectors(temp.path(), "thread", &artifact_id, vec![search])
        .await
        .expect("search canonical artifact");
    let result = &indexed.results[0];
    assert_eq!(result.status, ToolOutputSelectorStatus::Ok);
    assert!(!result.complete);
    assert_eq!(
        result.child_selectors,
        vec![ToolOutputSelector::Lines { start: 1, end: 5 }],
        "overlapping and adjacent match context should be coalesced",
    );
    assert_eq!(result.value.as_ref().unwrap()["total_matches"], 3);
    assert_eq!(result.value.as_ref().unwrap()["matches_returned"], 2);
    assert_eq!(result.value.as_ref().unwrap()["remaining_match_count"], 1);
    let hydrated = result.value.as_ref().unwrap()["hydrated_ranges"]
        .as_array()
        .expect("search hydrates exact context in the same call");
    assert_eq!(hydrated.len(), 1);
    assert!(
        hydrated[0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("needle alpha") && text.contains("needle beta"))
    );

    let selected = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        result.child_selectors.clone(),
    )
    .await
    .expect("select indexed match context in one batch");
    assert_eq!(selected.results.len(), 1);
    assert!(
        selected.results[0]
            .text
            .as_deref()
            .is_some_and(|text| text.contains("needle alpha") && text.contains("needle beta"))
    );

    let continuation = result.continuation.clone().expect("search continuation");
    let resumed =
        read_tool_output_selectors(temp.path(), "thread", &artifact_id, vec![continuation])
            .await
            .expect("resume canonical artifact search");
    let resumed = &resumed.results[0];
    assert!(resumed.complete);
    assert_eq!(resumed.value.as_ref().unwrap()["total_matches"], 1);
    assert_eq!(
        resumed.child_selectors,
        vec![ToolOutputSelector::Lines { start: 5, end: 7 }],
    );
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn artifact_recovery_search_page_fits_its_ceiling_and_advances() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text(
        (1..=200)
            .map(|line| format!("needle at historical line {line:04}\n"))
            .collect::<String>(),
    );
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");
    let search = ToolOutputSelector::Search {
        query: "needle".to_string(),
        start_byte: 0,
        max_results: ARTIFACT_SEARCH_MAX_RESULTS,
        context_lines: 0,
    };

    let indexed = read_tool_output_selectors_with_ceiling(
        temp.path(),
        "thread",
        &artifact_id,
        vec![search.clone()],
        512,
    )
    .await
    .expect("search canonical artifact within a nested ceiling");

    assert!(response_fits_recovery_token_ceiling(&indexed, 512));
    let result = &indexed.results[0];
    assert_eq!(result.status, ToolOutputSelectorStatus::Ok);
    let matches_returned = result.value.as_ref().unwrap()["matches_returned"]
        .as_u64()
        .expect("returned match count") as usize;
    assert!(matches_returned > 0);
    assert!(matches_returned < ARTIFACT_SEARCH_MAX_RESULTS);
    let continuation = result.continuation.as_ref().expect("search continuation");
    assert_ne!(
        continuation, &search,
        "pagination must advance after a fitting page"
    );
    assert!(matches!(
        continuation,
        ToolOutputSelector::Search { start_byte, .. } if *start_byte > 0
    ));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn artifact_recovery_sparse_search_avoids_a_historical_line_sweep() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text(
        (1..=1_082)
            .map(|line| {
                if matches!(line, 17 | 541 | 1_077) {
                    format!("historical line {line:04}: recovery target\n")
                } else {
                    format!("historical line {line:04}: ordinary output\n")
                }
            })
            .collect::<String>(),
    );
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");

    let indexed = read_tool_output_selectors_with_ceiling(
        temp.path(),
        "thread",
        &artifact_id,
        vec![ToolOutputSelector::Search {
            query: "recovery target".to_string(),
            start_byte: 0,
            max_results: ARTIFACT_SEARCH_DEFAULT_MAX_RESULTS,
            context_lines: 1,
        }],
        3_488,
    )
    .await
    .expect("search historical-sized artifact");
    let search = &indexed.results[0];
    assert_eq!(search.status, ToolOutputSelectorStatus::Ok);
    assert!(search.complete);
    assert_eq!(search.value.as_ref().unwrap()["total_matches"], 3);
    assert_eq!(search.child_selectors.len(), 3);
    assert!(
        search.value.as_ref().unwrap()["hydrated_ranges"]
            .as_array()
            .is_some_and(|ranges| {
                ranges.len() == 3
                    && ranges.iter().all(|range| {
                        range["text"]
                            .as_str()
                            .is_some_and(|text| text.contains("recovery target"))
                    })
            })
    );

    let selected = read_tool_output_selectors_with_ceiling(
        temp.path(),
        "thread",
        &artifact_id,
        search.child_selectors.clone(),
        3_488,
    )
    .await
    .expect("recover all sparse match context in one batch");
    assert!(response_fits_recovery_token_ceiling(&selected, 3_488));
    assert!(selected.results.iter().all(|result| {
        result.status == ToolOutputSelectorStatus::Ok
            && result
                .text
                .as_deref()
                .is_some_and(|text| text.contains("recovery target"))
    }));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
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
    assert!(
        selected
            .message
            .as_deref()
            .is_some_and(|message| message.contains("never the parent selector"))
    );
    let plan = selected
        .subdivision_plan
        .as_ref()
        .expect("bounded byte-subdivision plan");
    assert!(plan.chunk_bytes > 0);
    let continuation = selected
        .continuation
        .as_ref()
        .expect("host-directed bounded continuation");
    assert_eq!(selected.child_selectors.first(), Some(continuation));
    assert_eq!(
        continuation,
        &ToolOutputSelector::Bytes {
            start: expected_range.start,
            end: expected_range
                .start
                .saturating_add(plan.chunk_bytes)
                .min(expected_range.end),
        }
    );

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
#[serial_test::serial(command_output_artifact)]
async fn stall_nested_recovery_budget_returns_a_bounded_subdivision_plan() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text("source line\n".repeat(600));
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");

    let recovered = read_tool_output_selectors_with_ceiling(
        temp.path(),
        "thread",
        &artifact_id,
        vec![ToolOutputSelector::Lines { start: 1, end: 600 }],
        512,
    )
    .await
    .expect("bounded nested recovery");

    assert!(response_fits_recovery_token_ceiling(&recovered, 512));
    let selected = &recovered.results[0];
    assert_eq!(selected.status, ToolOutputSelectorStatus::SelectorTooLarge);
    assert!(selected.text.is_none());
    assert!(selected.subdivision_plan.is_some());
    assert!(selected.continuation.is_some());
    assert!(!recovered.complete);
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn stale_artifact_metadata_version_is_rejected_without_partial_success() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text("version identity\n");
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");
    let artifact_path = temp
        .path()
        .join("tool-output/thread")
        .join(format!("{artifact_id}.log"));
    let metadata_path = logical_metadata_path(&artifact_path);
    let mut metadata: Value =
        serde_json::from_slice(&std::fs::read(&metadata_path).expect("read logical metadata"))
            .expect("decode logical metadata");
    metadata["version"] = serde_json::json!(LOGICAL_ARTIFACT_METADATA_VERSION + 1);
    std::fs::write(
        metadata_path,
        serde_json::to_vec(&metadata).expect("encode stale metadata"),
    )
    .expect("write stale metadata version");

    let error = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        vec![ToolOutputSelector::Lines { start: 1, end: 1 }],
    )
    .await
    .expect_err("stale metadata version must fail the transaction");
    assert!(matches!(
        error,
        ReadToolOutputError::Io(message) if message.contains("metadata version")
    ));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn retained_prefix_recovery_reports_the_unavailable_canonical_suffix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text("retained\nunavailable\n");
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");
    let artifact_path = temp
        .path()
        .join("tool-output/thread")
        .join(format!("{artifact_id}.log"));
    let metadata_path = logical_metadata_path(&artifact_path);
    let retained = b"retained\n";
    std::fs::write(&artifact_path, retained).expect("truncate artifact to retained prefix");
    let mut metadata: LogicalArtifactMetadata =
        serde_json::from_slice(&std::fs::read(&metadata_path).expect("read logical metadata"))
            .expect("decode logical metadata");
    metadata.retained_bytes = retained.len() as u64;
    metadata.retained_sha256 = Some(format!("{:x}", Sha256::digest(retained)));
    metadata.complete = false;
    metadata.unavailable_ranges = vec![CanonicalByteRange::new(
        retained.len() as u64,
        canonical.exact_bytes,
    )];
    metadata.line_starts = canonical_line_starts(retained);
    metadata.segments[0].range = CanonicalByteRange::new(0, retained.len() as u64);
    std::fs::write(
        &metadata_path,
        serde_json::to_vec(&metadata).expect("encode partial metadata"),
    )
    .expect("write partial metadata");

    let recovered = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        vec![ToolOutputSelector::Bytes {
            start: 0,
            end: retained.len() as u64,
        }],
    )
    .await
    .expect("recover retained canonical prefix");

    assert!(recovered.complete);
    assert_eq!(recovered.retained_bytes, retained.len() as u64);
    assert_eq!(recovered.unavailable_ranges, metadata.unavailable_ranges);
    assert_eq!(recovered.results[0].text.as_deref(), Some("retained\n"));
    assert_eq!(
        recovered.results[0].canonical_range,
        Some(CanonicalByteRange::new(0, retained.len() as u64))
    );
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn stale_artifact_sha_is_rejected_without_partial_success() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text("sha identity\n");
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");
    let artifact_path = temp
        .path()
        .join("tool-output/thread")
        .join(format!("{artifact_id}.log"));
    let mut bytes = std::fs::read(&artifact_path).expect("read artifact segment");
    bytes[0] ^= 1;
    std::fs::write(&artifact_path, bytes).expect("replace artifact with same-sized stale bytes");

    let error = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        vec![ToolOutputSelector::Lines { start: 1, end: 1 }],
    )
    .await
    .expect_err("stale SHA must fail the transaction");
    assert!(matches!(
        error,
        ReadToolOutputError::Io(message) if message.contains("SHA identity")
    ));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn stale_artifact_size_is_rejected_without_partial_success() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text("size identity\n");
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("canonical artifact ID");
    let artifact_path = temp
        .path()
        .join("tool-output/thread")
        .join(format!("{artifact_id}.log"));
    let mut bytes = std::fs::read(&artifact_path).expect("read artifact segment");
    bytes.push(b'!');
    std::fs::write(&artifact_path, bytes).expect("replace artifact with stale size");

    let error = read_tool_output_selectors(
        temp.path(),
        "thread",
        &artifact_id,
        vec![ToolOutputSelector::Lines { start: 1, end: 1 }],
    )
    .await
    .expect_err("stale size must fail the transaction");
    assert!(matches!(
        error,
        ReadToolOutputError::Io(message) if message.contains("segment size")
    ));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
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
        remove_inactive_output_path_blocking(path.clone()),
        InactiveRemovalOutcome::Active
    ));
    assert!(path.exists());

    drop(active);
    assert!(matches!(
        remove_inactive_output_path_blocking(path.clone()),
        InactiveRemovalOutcome::RemovedOrAbsent
    ));
    assert!(!path.exists());
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
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
#[serial_test::serial(command_output_artifact)]
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
#[serial_test::serial(command_output_artifact)]
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

#[test]
fn active_tool_history_verification_bounds_reads_when_artifact_grows() {
    struct GrowingArtifact {
        read_bytes: usize,
    }

    impl Read for GrowingArtifact {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            assert!(buffer.len() <= 65_536, "read memory must remain bounded");
            self.read_bytes += buffer.len();
            assert!(
                self.read_bytes <= 131_074,
                "must stop after the growth probe"
            );
            buffer.fill(b'x');
            Ok(buffer.len())
        }
    }

    // The source keeps growing and never reaches EOF. The verifier must stop
    // after the declared size plus one byte without an allocation of that size.
    let mut artifact = GrowingArtifact { read_bytes: 0 };
    let result = verified_artifact_digest(&mut artifact, 131_073);

    assert_eq!(
        result,
        Err("artifact byte count does not match receipt metadata".to_string())
    );
    assert_eq!(artifact.read_bytes, 131_074);
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn active_tool_history_reconciliation_verifies_complete_bytes_before_protection() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = b"0123456789abcdef".repeat(16_385);
    // Independently computed SHA-256, including the final partial buffer.
    let digest = "7022bcb095d4bc64f8093af2f1d6cbb2f12ba573c7c2179563602e6ec6990fc5";
    let mut references = BTreeMap::new();
    let mut artifacts = Vec::new();
    for (bytes, sha256) in [
        (262_160, digest),
        (262_160, "invalid digest"),
        (262_159, digest),
    ] {
        let artifact = create_raw_output_artifact(temp.path(), "thread", &output).await;
        let RawOutputArtifact::Stored { id, path, .. } = &artifact else {
            panic!("expected stored artifact");
        };
        references.insert(id.to_string(), (bytes, sha256.to_string()));
        artifacts.push((id.to_string(), path.clone(), artifact));
    }

    let live =
        reconcile_active_tool_history_artifact_protection(temp.path(), "thread", &references).await;

    assert_eq!(live, BTreeSet::from([artifacts[0].0.clone()]));
    assert!(active_tool_history_protection_path(&artifacts[0].1).exists());
    for (_, path, _) in &artifacts[1..] {
        assert!(!active_tool_history_protection_path(path).exists());
        assert_eq!(
            tokio::fs::read(path).await.expect("retained artifact"),
            output
        );
    }
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn active_tool_history_artifact_survives_per_thread_retention_without_reducing_generic_limit()
{
    let temp = tempfile::tempdir().expect("tempdir");
    let output = b"active tool output";
    let artifact = create_raw_output_artifact(temp.path(), "thread", output).await;
    let RawOutputArtifact::Stored {
        id, path, bytes, ..
    } = &artifact
    else {
        panic!("expected stored artifact");
    };
    let id = id.to_string();
    let path = path.clone();
    protect_active_tool_history_artifact(
        temp.path(),
        "thread",
        &id,
        *bytes,
        &format!("{:x}", Sha256::digest(output)),
    )
    .await
    .expect("protect active tool-history artifact");
    drop(artifact);

    for index in 0..(max_retained_artifacts_per_thread() + 5) {
        create_raw_output_artifact(temp.path(), "thread", format!("generic-{index}").as_bytes())
            .await;
    }

    assert!(path.exists());
    assert!(active_tool_history_protection_path(&path).exists());
    let mut generic_logs = 0;
    let mut total_logs = 0;
    let mut entries = tokio::fs::read_dir(path.parent().expect("thread directory"))
        .await
        .expect("thread artifacts");
    while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
        if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
            total_logs += 1;
            if entry.path() != path {
                generic_logs += 1;
            }
        }
    }
    assert_eq!(generic_logs, max_retained_artifacts_per_thread());
    assert_eq!(total_logs, max_retained_artifacts_per_thread() + 1);
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn global_retention_skips_active_tool_history_without_broadening_generic_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = b"active tool output";
    let artifact = create_raw_output_artifact(temp.path(), "active-thread", output).await;
    let RawOutputArtifact::Stored {
        id, path, bytes, ..
    } = &artifact
    else {
        panic!("expected stored artifact");
    };
    let id = id.to_string();
    let protected_path = path.clone();
    protect_active_tool_history_artifact(
        temp.path(),
        "active-thread",
        &id,
        *bytes,
        &format!("{:x}", Sha256::digest(output)),
    )
    .await
    .expect("protect active tool-history artifact");
    drop(artifact);
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

    assert!(protected_path.exists());
    let mut generic_logs = 0;
    let mut directories = tokio::fs::read_dir(&root).await.expect("tool output root");
    while let Some(directory) = directories.next_entry().await.expect("thread directory") {
        let mut entries = tokio::fs::read_dir(directory.path())
            .await
            .expect("thread artifacts");
        while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
            if entry.path().extension().and_then(|value| value.to_str()) == Some("log")
                && entry.path() != protected_path
            {
                generic_logs += 1;
            }
        }
    }
    assert_eq!(generic_logs, max_retained_artifacts_total());
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
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
    let root = tool_output_root_for_directory(&directory);
    let retention_permit = retention_sweep_permit(&root)
        .await
        .expect("retention permit");
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

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn independent_retention_roots_acquire_in_parallel() {
    let temp = tempfile::tempdir().expect("tempdir");
    let first_root = temp.path().join("first").join("tool-output");
    let second_root = temp.path().join("second").join("tool-output");

    let first_permit = retention_sweep_permit(&first_root)
        .await
        .expect("first retention permit");
    let second_permit = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        retention_sweep_permit(&second_root),
    )
    .await
    .expect("an independent artifact root must not wait for the first root")
    .expect("second retention permit");

    drop(second_permit);
    drop(first_permit);
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn artifact_creation_fails_open_when_retention_lock_acquisition_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let failures_before = retention_sweep_permit_failures_for_test();
    inject_retention_sweep_permit_failure_for_test(1);

    let artifact = create_raw_output_artifact(temp.path(), "thread", b"durable output").await;

    assert!(matches!(artifact, RawOutputArtifact::Stored { .. }));
    assert_eq!(
        retention_sweep_permit_failures_for_test(),
        failures_before + 1,
        "the skipped sweep must remain observable"
    );
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn artifact_creation_fails_open_when_retention_lock_task_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let failures_before = retention_sweep_permit_failures_for_test();
    inject_retention_sweep_permit_failure_for_test(2);

    let artifact = create_raw_output_artifact(temp.path(), "thread", b"durable output").await;

    assert!(matches!(artifact, RawOutputArtifact::Stored { .. }));
    assert_eq!(
        retention_sweep_permit_failures_for_test(),
        failures_before + 1,
        "the failed lock task must remain observable"
    );
}

#[test]
#[serial_test::serial(command_output_artifact)]
fn audit_output_artifact_creation_reconciles_uncommitted_family() {
    let temp = tempfile::tempdir().expect("tempdir");
    let directory = temp.path().join("tool-output").join("thread");
    std::fs::create_dir_all(&directory).expect("artifact directory");
    let id = ToolOutputArtifactId::new();
    let path = directory.join(format!("{id}.log"));
    std::fs::write(&path, b"uncommitted base").expect("uncommitted base segment");
    std::fs::write(logical_segment_path(&path, 1), b"uncommitted tail")
        .expect("uncommitted tail segment");
    begin_logical_artifact_transaction(&path, false).expect("creation transaction");

    assert_eq!(
        load_logical_metadata(&path, id),
        Err(ReadToolOutputError::StillWriting),
        "a family with an active transaction must never fall back to a complete raw artifact"
    );
    reconcile_logical_artifact_transactions(&directory).expect("reconcile transactions");

    assert!(!path.exists());
    assert!(!logical_segment_path(&path, 1).exists());
    assert!(!logical_transaction_path(&path).exists());
}

#[test]
#[serial_test::serial(command_output_artifact)]
fn audit_output_artifact_attach_rollback_preserves_only_base_segment() {
    let temp = tempfile::tempdir().expect("tempdir");
    let directory = temp.path().join("tool-output").join("thread");
    std::fs::create_dir_all(&directory).expect("artifact directory");
    let path = directory.join(format!("{}.log", ToolOutputArtifactId::new()));
    std::fs::write(&path, b"legacy exact output").expect("legacy base segment");
    begin_logical_artifact_transaction(&path, true).expect("attach transaction");
    std::fs::write(logical_segment_path(&path, 1), b"uncommitted tail")
        .expect("new segment installed before metadata failure");

    reconcile_logical_artifact_transaction(&path).expect("rollback failed attach");

    assert_eq!(
        std::fs::read(&path).expect("preserved base"),
        b"legacy exact output"
    );
    assert!(!logical_segment_path(&path, 1).exists());
    assert!(!logical_transaction_path(&path).exists());
}

#[test]
fn audit_output_artifact_unavailable_ranges_preserve_canonical_gaps() {
    assert_eq!(
        normalized_unavailable_ranges(12, &[CanonicalByteRange::new(2, 4)], 12),
        vec![CanonicalByteRange::new(2, 4)]
    );
    assert_eq!(
        normalized_unavailable_ranges(
            12,
            &[
                CanonicalByteRange::new(2, 4),
                CanonicalByteRange::new(7, 10),
            ],
            8,
        ),
        vec![
            CanonicalByteRange::new(2, 4),
            CanonicalByteRange::new(7, 12)
        ]
    );
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn audit_output_artifact_retention_permit_uses_interprocess_lock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("tool-output");
    let permit = retention_sweep_permit(&root)
        .await
        .expect("retention permit");
    let _ = take_retention_interprocess_index_stale(&root);
    let generation_path = retention_interprocess_lock_target(&root);
    let lock_path =
        codex_file_system::atomic_write_lock_path(&generation_path).expect("retention lock path");
    let contender = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .expect("contending retention lock handle");

    assert!(
        fs2::FileExt::try_lock_exclusive(&contender).is_err(),
        "another process handle must not enter retention while the permit is held"
    );
    drop(permit);
    fs2::FileExt::try_lock_exclusive(&contender)
        .expect("interprocess lock should release with the retention permit");
    let external_generation = std::fs::read_to_string(&generation_path)
        .expect("retention generation")
        .trim()
        .parse::<u64>()
        .expect("numeric retention generation")
        .checked_add(1)
        .expect("external generation increment");
    write_bytes_atomically(&generation_path, external_generation.to_string().as_bytes())
        .expect("simulate another process mutation");
    fs2::FileExt::unlock(&contender).expect("unlock contender");

    let next_permit = retention_sweep_permit(&root)
        .await
        .expect("next retention permit");
    assert!(
        take_retention_interprocess_index_stale(&root),
        "a generation advanced by another process must invalidate the local index"
    );
    drop(next_permit);
}

#[test]
#[serial_test::serial(command_output_artifact)]
fn malformed_or_overflowed_retention_generation_is_reinitialized() {
    let temp = tempfile::tempdir().expect("tempdir");
    for (name, contents) in [
        ("malformed", "not-a-generation".to_string()),
        ("overflowed", u64::MAX.to_string()),
    ] {
        let generation_path = temp.path().join(name);
        let root = temp.path().join(format!("{name}-root"));
        std::fs::write(&generation_path, contents).expect("write invalid generation");
        let _ = take_retention_interprocess_index_stale(&root);

        advance_retention_interprocess_generation(&root, &generation_path)
            .expect("invalid generation should be recoverable");

        assert_eq!(
            std::fs::read_to_string(&generation_path).expect("read repaired generation"),
            "1"
        );
        assert!(
            take_retention_interprocess_index_stale(&root),
            "repairing shared generation metadata must invalidate the local index"
        );
    }
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
#[serial_test::serial(command_output_artifact)]
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
#[serial_test::serial(command_output_artifact)]
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
#[serial_test::serial(command_output_artifact)]
async fn streaming_finish_does_not_publish_success_when_file_sync_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"").await;
    let state = Arc::new(Mutex::new(artifact));
    let mut writer = RawOutputArtifactWriter::open(Some(&state))
        .await
        .expect("streaming writer");
    writer.write_chunk(Some(&state), b"durable output").await;

    inject_streaming_finalize_sync_failure_for_test();
    writer.finish(Some(&state)).await;

    let artifact = state.lock().await;
    assert!(matches!(
        &*artifact,
        RawOutputArtifact::Failed { message, .. }
            if message.contains("failed to sync")
                && message.contains("injected streaming output sync failure")
    ));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
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
#[serial_test::serial(command_output_artifact)]
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
#[serial_test::serial(command_output_artifact)]
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
        LogicalRetentionMutation::ProtectionReconcile,
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
#[serial_test::serial(command_output_artifact)]
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
#[serial_test::serial(command_output_artifact)]
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
#[serial_test::serial(command_output_artifact)]
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
#[serial_test::serial(command_output_artifact)]
async fn generation_is_not_reused_after_root_eviction_and_reinitialization() {
    let temp = tempfile::tempdir().expect("tempdir");
    let roots = (0..=MAX_RETENTION_INDEX_ROOTS)
        .map(|index| temp.path().join(format!("tool-output-{index}")))
        .collect::<Vec<_>>();
    let mut registry = RetentionRegistry::default();
    let first = insert_dirty_root(&mut registry, roots[0].clone()).expect("first generation");
    for root in &roots[1..MAX_RETENTION_INDEX_ROOTS] {
        insert_dirty_root(&mut registry, root.clone()).expect("root generation");
    }
    assert_eq!(registry.roots.len(), MAX_RETENTION_INDEX_ROOTS);

    insert_dirty_root(&mut registry, roots[MAX_RETENTION_INDEX_ROOTS].clone())
        .expect("evicting generation");
    assert!(!registry.roots.contains_key(&roots[0]));
    let second =
        insert_dirty_root(&mut registry, roots[0].clone()).expect("reinitialized generation");

    assert_ne!(first, second);
    assert_eq!(MAX_RETENTION_INDEX_ROOTS, 4);
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn invalid_protection_marker_fails_reconciliation_open() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"artifact").await;
    let RawOutputArtifact::Stored { path, .. } = artifact else {
        panic!("expected stored artifact");
    };
    std::fs::write(
        active_tool_history_protection_path(&path),
        b"invalid marker",
    )
    .expect("write invalid marker");
    let root = temp.path().join("tool-output");

    assert_eq!(
        force_retention_reconciliation_for_test(&root).await,
        RetentionModeKind::Dirty
    );
    assert_eq!(retention_mode_for_test(&root), RetentionModeKind::Dirty);
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
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
#[serial_test::serial(command_output_artifact)]
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
#[serial_test::serial(command_output_artifact)]
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

#[cfg(windows)]
#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn canonical_creation_preserves_committed_family_when_recovery_metadata_is_unreadable() {
    use std::os::windows::fs::OpenOptionsExt;
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = CanonicalToolResult::text("previous committed exact output\n");
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    assert!(artifact.complete, "{artifact:?}");
    let id = artifact.artifact_id().expect("artifact ID");
    let path = temp
        .path()
        .join("tool-output/thread")
        .join(format!("{id}.log"));
    // This is the on-disk state after metadata was committed but before the
    // transaction marker was removed. Recovery is entered by public creation.
    begin_logical_artifact_transaction(&path, false).expect("unfinished transaction marker");
    let locked_metadata = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(logical_metadata_path(&path))
        .expect("hold actual metadata sharing lock");
    let failed = create_canonical_output_artifact(
        temp.path(),
        "thread",
        &CanonicalToolResult::text("new output while recovery is blocked"),
    )
    .await;
    assert!(!failed.complete);
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("reconcile artifact transactions"))
    );
    assert_eq!(
        std::fs::read(&path).expect("committed segment preserved"),
        canonical.bytes
    );
    assert!(
        logical_transaction_path(&path).exists(),
        "failed recovery must retain its marker"
    );
    drop(locked_metadata);
    let retried = create_canonical_output_artifact(
        temp.path(),
        "thread",
        &CanonicalToolResult::text("creation resumes after metadata is readable"),
    )
    .await;
    assert!(retried.complete, "{retried:?}");
    assert!(!logical_transaction_path(&path).exists());
    let recovered = read_tool_output_selectors(
        temp.path(),
        "thread",
        &id,
        vec![ToolOutputSelector::Lines { start: 1, end: 1 }],
    )
    .await
    .expect("read preserved family");
    assert!(recovered.complete);
    assert_eq!(
        recovered.results[0].text.as_deref(),
        Some("previous committed exact output\n")
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(command_output_artifact)]
async fn cancelled_history_protection_retains_admission_until_protection_is_visible() {
    let temp = tempfile::tempdir().expect("artifact home");
    let body = "history protection survives caller cancellation\n";
    let artifact =
        create_canonical_output_artifact(temp.path(), "thread", &CanonicalToolResult::text(body))
            .await;
    let id = artifact.artifact_id().expect("canonical artifact");
    let root = temp.path().join("tool-output");
    let path = root.join("thread").join(format!("{id}.log"));
    let marker = active_tool_history_protection_path(&path);
    let references = BTreeMap::from([(
        id.clone(),
        (
            body.len() as u64,
            format!("{:x}", Sha256::digest(body.as_bytes())),
        ),
    )]);
    let semaphore = retention_sweep_semaphore(&root);
    // Real retention-lock I/O remains blocked after the normal reconciliation caller
    // is cancelled. The process admission must stay owned by that in-flight work.
    let external_lock = acquire_atomic_write_lock(&retention_interprocess_lock_target(&root))
        .expect("hold external retention lock");
    let operation = tokio::spawn({
        let home = temp.path().to_path_buf();
        let references = references.clone();
        async move {
            reconcile_active_tool_history_artifact_protection(&home, "thread", &references).await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while semaphore.available_permits() != 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("normal reconciliation reaches retention admission");
    assert!(
        !marker.exists(),
        "blocked filesystem acquisition cannot publish protection"
    );
    operation.abort();
    assert!(
        operation
            .await
            .expect_err("cancel reconciliation caller")
            .is_cancelled()
    );
    assert!(
        semaphore.clone().try_acquire_owned().is_err(),
        "cancelling caller must not admit a second retention owner while filesystem work continues"
    );
    assert_eq!(
        tokio::fs::read(&path).await.expect("read retained bytes"),
        body.as_bytes()
    );
    drop(external_lock);
    let permit = tokio::time::timeout(Duration::from_secs(5), semaphore.clone().acquire_owned())
        .await
        .expect("owned worker finishes")
        .expect("retention admission released");
    assert!(
        protection_marker_status(&marker, ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES)
            .expect("read durable protection marker")
    );
    let record = artifact_retention_record(&path)
        .await
        .expect("read published artifact record")
        .expect("artifact retained");
    assert!(
        record.protected,
        "admission releases after protection is visible to retention"
    );
    drop(permit);
    let recovered = read_tool_output_selectors(
        temp.path(),
        "thread",
        &id,
        vec![ToolOutputSelector::Bytes {
            start: 0,
            end: body.len() as u64,
        }],
    )
    .await
    .expect("recover protected artifact");
    assert!(recovered.complete);
    assert_eq!(recovered.results[0].text.as_deref(), Some(body));
    let live =
        reconcile_active_tool_history_artifact_protection(temp.path(), "thread", &references).await;
    assert_eq!(
        live,
        BTreeSet::from([id]),
        "ordinary retry sees completed protection without stranding retention"
    );
}

async fn create_protected_pruning_fixture(
    codex_home: &Path,
) -> (PathBuf, PathBuf, BTreeMap<String, (u64, String)>) {
    let mut markers = Vec::new();
    let mut referenced = BTreeMap::new();
    for (index, text) in ["old canonical output\n", "referenced canonical output\n"]
        .into_iter()
        .enumerate()
    {
        let canonical = CanonicalToolResult::text(text);
        let artifact = create_canonical_output_artifact(codex_home, "thread", &canonical).await;
        assert!(artifact.complete, "{artifact:?}");
        let id = artifact.artifact_id().expect("canonical artifact id");
        protect_active_tool_history_artifact(
            codex_home,
            "thread",
            &id,
            canonical.exact_bytes,
            &canonical.sha256,
        )
        .await
        .expect("protect actual canonical artifact");
        let path = codex_home
            .join("tool-output/thread")
            .join(format!("{id}.log"));
        markers.push(active_tool_history_protection_path(&path));
        if index == 1 {
            referenced.insert(id, (canonical.exact_bytes, canonical.sha256));
        }
    }
    (markers.remove(0), markers.remove(0), referenced)
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn committed_history_pruning_retains_referenced_markers_without_revalidating_bytes() {
    for missing in [true, false] {
        let temp = tempfile::tempdir().expect("tempdir");
        let (obsolete, referenced, references) =
            create_protected_pruning_fixture(temp.path()).await;
        let expected_marker = if missing {
            tokio::fs::remove_file(referenced.with_extension("log"))
                .await
                .expect("remove referenced artifact bytes");
            ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES
        } else {
            tokio::fs::write(referenced.with_extension("log"), b"wrong artifact bytes")
                .await
                .expect("change referenced bytes");
            tokio::fs::write(&referenced, b"invalid marker bytes")
                .await
                .expect("change referenced marker bytes");
            b"invalid marker bytes".as_slice()
        };

        prune_active_tool_history_artifact_protection(temp.path(), "thread", &references)
            .await
            .expect("pruning does not revalidate the committed reference set");

        assert!(!obsolete.exists(), "obsolete protection must be removed");
        assert_eq!(
            tokio::fs::read(&referenced)
                .await
                .expect("referenced marker must remain"),
            expected_marker
        );
        assert_eq!(
            tokio::fs::read(obsolete.with_extension("log"))
                .await
                .expect("pruning preserves artifact bytes"),
            b"old canonical output\n"
        );
        if missing {
            assert!(
                !referenced.with_extension("log").exists(),
                "pruning must not recreate artifact bytes"
            );
        } else {
            assert_eq!(
                tokio::fs::read(referenced.with_extension("log"))
                    .await
                    .unwrap(),
                b"wrong artifact bytes"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn committed_history_pruning_accepts_absent_directory_but_reports_unreadable_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    prune_active_tool_history_artifact_protection(temp.path(), "thread", &BTreeMap::new())
        .await
        .expect("absent artifact directory has no markers to prune");
    let root = temp.path().join("tool-output");
    tokio::fs::create_dir_all(&root)
        .await
        .expect("artifact root");
    let directory = root.join("thread");
    tokio::fs::write(&directory, "not a directory")
        .await
        .expect("real read-directory failure");
    let error =
        prune_active_tool_history_artifact_protection(temp.path(), "thread", &BTreeMap::new())
            .await
            .expect_err("an existing unreadable directory path must not count as empty");
    assert!(error.contains("protection directory"));
    assert_eq!(
        tokio::fs::read(&directory).await.unwrap(),
        b"not a directory"
    );
}

#[cfg(windows)]
#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn committed_history_pruning_reports_denied_delete_and_retries_after_repair() {
    use std::os::windows::fs::OpenOptionsExt;
    let temp = tempfile::tempdir().expect("tempdir");
    let (obsolete, referenced, references) = create_protected_pruning_fixture(temp.path()).await;
    // Permit ordinary readers/writers but deny delete sharing on the obsolete
    // marker. This is a real filesystem failure, not a fake pruning operation.
    let deny_delete = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2)
        .open(&obsolete)
        .expect("hold obsolete marker without delete sharing");
    let error = prune_active_tool_history_artifact_protection(temp.path(), "thread", &references)
        .await
        .expect_err("failed deletion must fail the checkpoint cleanup barrier");
    assert!(error.contains("failed to remove obsolete active tool-history protection"));
    assert!(obsolete.exists());
    assert_eq!(
        tokio::fs::read(&referenced).await.unwrap(),
        ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES
    );
    drop(deny_delete);

    prune_active_tool_history_artifact_protection(temp.path(), "thread", &references)
        .await
        .expect("explicit retry completes after filesystem repair");
    assert!(!obsolete.exists());
    assert_eq!(
        tokio::fs::read(&referenced).await.unwrap(),
        ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES
    );
    assert_eq!(
        tokio::fs::read(referenced.with_extension("log"))
            .await
            .unwrap(),
        b"referenced canonical output\n"
    );
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn committed_history_pruning_cancellation_keeps_worker_and_retention_lock_owned() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (obsolete, referenced, references) = create_protected_pruning_fixture(temp.path()).await;
    let directory = referenced.parent().expect("thread directory");
    let root = tool_output_root_for_directory(directory);
    let semaphore = retention_sweep_semaphore(&root);
    let external_lock = acquire_atomic_write_lock(&retention_interprocess_lock_target(&root))
        .expect("hold external retention lock");
    let operation = tokio::spawn({
        let home = temp.path().to_path_buf();
        async move { prune_active_tool_history_artifact_protection(&home, "thread", &references).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while semaphore.available_permits() != 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("pruning worker owns retention admission");
    operation.abort();
    assert!(
        operation
            .await
            .expect_err("caller is cancelled")
            .is_cancelled()
    );
    assert!(
        obsolete.exists(),
        "filesystem pruning is blocked by the OS lock"
    );
    assert!(
        semaphore.clone().try_acquire_owned().is_err(),
        "caller cancellation must not release the worker's admission"
    );
    drop(external_lock);
    let completed = tokio::time::timeout(Duration::from_secs(5), semaphore.clone().acquire_owned())
        .await
        .expect("accepted worker completes without its caller")
        .expect("retention gate remains usable");
    assert!(
        !obsolete.exists(),
        "completed worker pruned obsolete protection"
    );
    let record = artifact_retention_record(&obsolete.with_extension("log"))
        .await
        .expect("read updated retention record")
        .expect("old artifact is retained");
    assert!(
        !record.protected,
        "admission releases after the removal is visible to retention"
    );
    drop(completed);
    assert_eq!(
        tokio::fs::read(&referenced).await.unwrap(),
        ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES
    );
    assert_eq!(
        tokio::fs::read(referenced.with_extension("log"))
            .await
            .unwrap(),
        b"referenced canonical output\n"
    );
}

#[test]
#[serial_test::serial(command_output_artifact)]
fn history_protection_finishes_with_one_blocking_thread() {
    let temp = tempfile::tempdir().expect("artifact home");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single blocking worker runtime");
    let result = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let body = b"single-worker protected output\n";
            let artifact = create_raw_output_artifact(temp.path(), "thread", body).await;
            let RawOutputArtifact::Stored { id, path, .. } = artifact else {
                panic!("normal raw artifact creation must complete with one blocking worker");
            };
            protect_active_tool_history_artifact(
                temp.path(),
                "thread",
                &id.to_string(),
                body.len() as u64,
                &format!("{:x}", Sha256::digest(body)),
            )
            .await
            .expect("normal public protection completes");
            let marker = active_tool_history_protection_path(&path);
            assert!(
                protection_marker_status(&marker, ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES)
                    .expect("valid protection marker")
            );
            assert_eq!(
                tokio::fs::read(&path).await.expect("retained exact bytes"),
                body
            );
            let record = artifact_retention_record(&path)
                .await
                .expect("retention record")
                .expect("artifact remains");
            assert!(record.protected);
        })
        .await
    });
    // A regressed nested blocking wait must fail this test rather than hang runtime Drop.
    runtime.shutdown_timeout(Duration::from_millis(100));
    result.expect("public protection must not require a second blocking worker");
}

#[test]
#[serial_test::serial(command_output_artifact)]
fn committed_history_pruning_finishes_with_one_blocking_thread() {
    let temp = tempfile::tempdir().expect("artifact home");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single blocking worker runtime");
    let result = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut markers = Vec::new();
            let mut references = BTreeMap::new();
            for (index, body) in [b"old output".as_slice(), b"referenced output".as_slice()]
                .into_iter()
                .enumerate()
            {
                let artifact = create_raw_output_artifact(temp.path(), "thread", body).await;
                let RawOutputArtifact::Stored { id, path, .. } = artifact else {
                    panic!("raw artifact creation should complete");
                };
                let digest = format!("{:x}", Sha256::digest(body));
                protect_active_tool_history_artifact(
                    temp.path(),
                    "thread",
                    &id.to_string(),
                    body.len() as u64,
                    &digest,
                )
                .await
                .expect("protect real artifact");
                markers.push(active_tool_history_protection_path(&path));
                if index == 1 {
                    references.insert(id.to_string(), (body.len() as u64, digest));
                }
            }
            prune_active_tool_history_artifact_protection(temp.path(), "thread", &references)
                .await
                .expect("prune does not require another blocking worker");
            assert!(!markers[0].exists());
            assert_eq!(
                tokio::fs::read(&markers[1]).await.unwrap(),
                ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES
            );
            let obsolete = artifact_retention_record(&markers[0].with_extension("log"))
                .await
                .expect("indexed old artifact")
                .expect("old bytes retained");
            assert!(!obsolete.protected);
            let current = artifact_retention_record(&markers[1].with_extension("log"))
                .await
                .expect("indexed referenced artifact")
                .expect("referenced bytes retained");
            assert!(current.protected);
        })
        .await
    });
    runtime.shutdown_timeout(Duration::from_millis(100));
    result.expect("pruning must complete with one blocking worker");
}

#[test]
#[serial_test::serial(command_output_artifact)]
fn canonical_creation_finishes_with_one_blocking_thread() {
    assert_canonical_operation_finishes_with_one_blocking_thread(false);
}

#[test]
#[serial_test::serial(command_output_artifact)]
fn canonical_attachment_finishes_with_one_blocking_thread() {
    assert_canonical_operation_finishes_with_one_blocking_thread(true);
}

fn assert_canonical_operation_finishes_with_one_blocking_thread(attach: bool) {
    let temp = tempfile::tempdir().expect("artifact home");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single blocking worker runtime");
    let result = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let body = "a".repeat(MAX_RAW_OUTPUT_ARTIFACT_BYTES) + &"b".repeat(128);
            let canonical = CanonicalToolResult::text(body.clone());
            let raw = if attach {
                Some(create_raw_output_artifact(temp.path(), "thread", body.as_bytes()).await)
            } else {
                None
            };
            let raw_id = raw
                .as_ref()
                .map(|raw| raw.artifact_id().expect("stored raw ID").to_string());

            // Failed admission must clean completed staging and preserve an existing raw family.
            inject_retention_sweep_permit_failure_for_test(1);
            let failed = match &raw_id {
                Some(id) => {
                    attach_canonical_output_artifact(temp.path(), "thread", id, &canonical).await
                }
                None => create_canonical_output_artifact(temp.path(), "thread", &canonical).await,
            };
            assert!(!failed.complete);
            assert!(
                failed
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("retention lock"))
            );
            let retained_prefix = if attach {
                MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64
            } else {
                0
            };
            assert_eq!(failed.retained_bytes, retained_prefix);
            assert_eq!(
                failed.unavailable_ranges,
                vec![CanonicalByteRange::new(retained_prefix, body.len() as u64)]
            );
            let directory = temp.path().join("tool-output/thread");
            for entry in std::fs::read_dir(&directory).expect("staging directory") {
                let entry = entry.expect("entry");
                if let Some(id) = &raw_id {
                    assert_eq!(entry.file_name().to_string_lossy(), format!("{id}.log"));
                    assert_eq!(
                        std::fs::read(entry.path()).expect("raw bytes"),
                        body.as_bytes()[..MAX_RAW_OUTPUT_ARTIFACT_BYTES]
                    );
                } else {
                    panic!(
                        "failed create left a family member: {}",
                        entry.path().display()
                    );
                }
            }

            let artifact = match &raw_id {
                Some(id) => {
                    attach_canonical_output_artifact(temp.path(), "thread", id, &canonical).await
                }
                None => create_canonical_output_artifact(temp.path(), "thread", &canonical).await,
            };
            assert!(artifact.complete, "{artifact:?}");
            assert_eq!(artifact.error, None);
            assert_eq!(artifact.retained_bytes, body.len() as u64);
            assert!(artifact.unavailable_ranges.is_empty());
            let id = artifact.artifact_id().expect("canonical ID");
            if let Some(raw_id) = raw_id {
                assert_eq!(id, raw_id, "attachment preserves the public handle");
            }
            let path = directory.join(format!("{id}.log"));
            let metadata: LogicalArtifactMetadata = serde_json::from_slice(
                &std::fs::read(logical_metadata_path(&path)).expect("committed metadata"),
            )
            .expect("logical metadata");
            assert_eq!(metadata.segments.len(), 2);
            assert_eq!(metadata.canonical_bytes, body.len() as u64);
            assert_eq!(
                metadata.canonical_sha256,
                format!("{:x}", Sha256::digest(body.as_bytes()))
            );
            let mut persisted = std::fs::read(&path).expect("base segment");
            persisted.extend(std::fs::read(logical_segment_path(&path, 1)).expect("tail segment"));
            assert_eq!(persisted, body.as_bytes());
            let recovered = read_tool_output_selectors(
                temp.path(),
                "thread",
                &id,
                vec![ToolOutputSelector::Bytes {
                    start: MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64 - 32,
                    end: MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64 + 32,
                }],
            )
            .await
            .expect("normal reader crosses segment boundary");
            assert!(recovered.complete);
            assert_eq!(
                recovered.results[0].text.as_deref(),
                Some(("a".repeat(32) + &"b".repeat(32)).as_str())
            );
            let record = artifact_retention_record(&path)
                .await
                .expect("retention read")
                .expect("retained family");
            assert!(record.bytes >= body.len() as u64);
            let permit = retention_sweep_permit_for_directory(&directory)
                .await
                .expect("released retention ownership");
            for entry in std::fs::read_dir(&directory).expect("committed directory") {
                let name = entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned();
                assert!(
                    !name.ends_with(".pending") && !name.ends_with(".transaction"),
                    "uncommitted member: {name}"
                );
            }
            drop(permit);
        })
        .await
    });
    runtime.shutdown_timeout(Duration::from_millis(100));
    result.expect("public canonical operation must not require a second blocking worker");
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(command_output_artifact)]
async fn cancelled_canonical_attachment_finishes_owned_family_and_releases_retention() {
    let temp = tempfile::tempdir().expect("artifact home");
    let body = "a".repeat(MAX_RAW_OUTPUT_ARTIFACT_BYTES) + "attached tail\n";
    let canonical = CanonicalToolResult::text(body.clone());
    let raw = create_raw_output_artifact(temp.path(), "thread", body.as_bytes()).await;
    let id = raw.artifact_id().expect("raw ID");
    let directory = temp.path().join("tool-output/thread");
    let root = directory.parent().expect("output root");
    let permit = retention_sweep_semaphore(root)
        .acquire_owned()
        .await
        .expect("hold admission");
    let home = temp.path().to_path_buf();
    let operation = tokio::spawn(async move {
        attach_canonical_output_artifact(&home, "thread", &id.to_string(), &canonical).await
    });
    let pending = staged_logical_segment_path(&directory, id, 1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !pending.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("attachment stages its additional segment before admission");
    operation.abort();
    assert!(
        operation
            .await
            .expect_err("cancel public caller")
            .is_cancelled()
    );
    drop(permit);
    let recovered = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(recovered) = read_tool_output_selectors(
                temp.path(),
                "thread",
                &id.to_string(),
                vec![ToolOutputSelector::Bytes {
                    start: MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64,
                    end: body.len() as u64,
                }],
            )
            .await
                && recovered.complete
                && recovered.results[0].text.as_deref() == Some("attached tail\n")
            {
                break recovered;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("owned attachment commits after caller cancellation");
    assert_eq!(
        recovered.results[0].text.as_deref(),
        Some("attached tail\n")
    );
    let permit = retention_sweep_permit_for_directory(&directory)
        .await
        .expect("attachment releases retention");
    assert!(!pending.exists());
    let path = directory.join(format!("{id}.log"));
    assert_eq!(
        std::fs::read(&path).expect("preserved base"),
        body.as_bytes()[..MAX_RAW_OUTPUT_ARTIFACT_BYTES]
    );
    assert!(!logical_transaction_path(&path).exists());
    drop(permit);
}
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(command_output_artifact)]
async fn cancelled_raw_stream_open_releases_lock_before_returning_writer() {
    let temp = tempfile::tempdir().expect("artifact home");
    let artifact =
        create_raw_output_artifact(temp.path(), "thread", b"original retained bytes\n").await;
    let RawOutputArtifact::Stored { id, path, .. } = &artifact else {
        panic!("raw artifact creation failed");
    };
    let id = id.to_string();
    let path = path.clone();
    let state = Arc::new(Mutex::new(artifact));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let operation = tokio::spawn({
        let state = Arc::clone(&state);
        let barrier = Arc::clone(&barrier);
        async move {
            let _writer = RAW_OUTPUT_LOCK_BARRIER_FOR_TEST
                .scope(barrier, RawOutputArtifactWriter::open(Some(&state)))
                .await;
        }
    });
    tokio::time::timeout(Duration::from_secs(5), barrier.wait())
        .await
        .expect("normal streaming open acquires its output lock");
    let contender = File::options()
        .read(true)
        .write(true)
        .open(&path)
        .expect("independent artifact handle");
    assert!(matches!(
        contender.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    operation.abort();
    assert!(operation.await.expect_err("cancel caller").is_cancelled());
    barrier.wait().await;

    let unlocked = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match contender.try_lock() {
                Ok(()) => break true,
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("artifact lock failed: {error}"),
            }
        }
    })
    .await;
    // Release the original owner even on a failed assertion so a leaking
    // implementation does not leave this scenario's fixture locked.
    if unlocked.is_err() {
        if let RawOutputArtifact::Stored { handle, .. } = &*state.lock().await {
            let _ = handle.unlock();
        }
    }
    assert!(
        unlocked.is_ok(),
        "cancelled lock handoff must release the shared lock"
    );
    contender.unlock().expect("release independent lock");
    assert!(matches!(
        &*state.lock().await,
        RawOutputArtifact::Stored { bytes: 24, .. }
    ));
    let recovered = read_tool_output_selectors(
        temp.path(),
        "thread",
        &id,
        vec![ToolOutputSelector::Bytes { start: 0, end: 24 }],
    )
    .await
    .expect("unchanged raw artifact remains recoverable");
    assert_eq!(
        recovered.results[0].text.as_deref(),
        Some("original retained bytes\n")
    );
}

#[test]
#[serial_test::serial(command_output_artifact)]
fn raw_retention_worker_keeps_ownership_after_caller_and_runtime_cancellation() {
    let temp = tempfile::tempdir().expect("artifact home");
    let directory = temp.path().join("tool-output/thread");
    let root = directory.parent().expect("output root");
    let semaphore = retention_sweep_semaphore(root);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    set_reconciliation_barrier(root, Arc::clone(&barrier));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("one blocking worker");
    let retained_after_caller_cancellation = runtime.block_on(async {
        let home = temp.path().to_path_buf();
        let operation = tokio::spawn(async move {
            create_raw_output_artifact(&home, "thread", b"retention survives runtime shutdown\n")
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), barrier.wait())
            .await
            .expect("normal raw create enters retention scan");
        operation.abort();
        assert!(
            operation
                .await
                .expect_err("cancel raw caller")
                .is_cancelled()
        );
        Arc::clone(&semaphore).try_acquire_owned().is_err()
    });
    runtime.shutdown_timeout(Duration::from_millis(100));
    let retained_after_runtime_cancellation = Arc::clone(&semaphore).try_acquire_owned().is_err();

    let reader_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("reader runtime");
    reader_runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), barrier.wait())
            .await
            .expect("release original worker's scan");
        let permit = tokio::time::timeout(Duration::from_secs(5), semaphore.acquire_owned())
            .await
            .expect("original worker completed")
            .expect("retention available");
        let paths = std::fs::read_dir(&directory)
            .expect("committed directory")
            .map(|entry| entry.expect("entry").path())
            .collect::<Vec<_>>();
        assert_eq!(paths.len(), 1);
        assert_eq!(
            std::fs::read(&paths[0]).expect("retained output"),
            b"retention survives runtime shutdown\n"
        );
        assert_eq!(retention_mode_for_test(root), RetentionModeKind::Indexed);
        let registry = lock_retention_registry();
        let state = registry
            .roots
            .get(&normalized_tool_output_root(root))
            .expect("published root");
        let RetentionRootMode::Indexed(index) = &state.mode else {
            panic!("completed index");
        };
        assert_eq!(index.records.len(), 1);
        assert_eq!(
            index.total_bytes,
            b"retention survives runtime shutdown\n".len() as u64
        );
        drop(registry);
        drop(permit);
        let id = paths[0]
            .file_stem()
            .expect("artifact ID")
            .to_str()
            .expect("UTF-8 ID");
        let output = read_tool_output_artifact(temp.path(), "thread", id, 1, 1, 16_384)
            .await
            .expect("normal reader after runtime shutdown");
        assert_eq!(
            output,
            format!(
                "artifact {id}, lines 1–1, 36 retained bytes\nretention survives runtime shutdown\n"
            )
        );
    });
    assert!(
        retained_after_caller_cancellation,
        "caller cancellation released the running filesystem worker's permit"
    );
    assert!(
        retained_after_runtime_cancellation,
        "runtime shutdown released the running filesystem worker's permit"
    );
}

#[test]
#[serial_test::serial(command_output_artifact)]
fn reduction_notice_queues_filesystem_work_and_rejects_deleted_artifacts() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single-worker runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("artifact home");
        let artifact = create_raw_output_artifact(temp.path(), "notice", b"recover these bytes\n").await;
        let RawOutputArtifact::Stored { path, .. } = &artifact else {
            panic!("normal artifact creation failed");
        };
        let path = path.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let released = Arc::new(AtomicBool::new(false));
        let worker_released = Arc::clone(&released);
        let occupied = tokio::task::spawn_blocking(move || {
            entered_tx.send(()).expect("worker occupied");
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
            worker_released.store(true, Ordering::Release);
        });
        entered_rx.recv().expect("sole blocking worker occupied");
        let notice = tokio::spawn(async move {
            let text = artifact.reduction_notice().await;
            (artifact, text)
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let runtime_advanced_before_worker_release = !released.load(Ordering::Acquire);
        let notice_waited_for_worker = !notice.is_finished();
        let _ = release_tx.send(());
        occupied.await.expect("blocking worker released");
        let (artifact, text) = notice.await.expect("notice task");
        assert!(runtime_advanced_before_worker_release);
        assert!(notice_waited_for_worker, "the actual path check must enter the blocking pool");
        assert_eq!(text.as_deref(), Some("[command output reduced; recover the full retained output with read_tool_output using the raw output artifact above. Batch exact ranges when possible; do not rerun the producer.]"));
        assert_eq!(tokio::fs::read(&path).await.expect("retained output"), b"recover these bytes\n");
        tokio::fs::remove_file(&path).await.expect("expire artifact");
        assert_eq!(artifact.reduction_notice().await, None, "expired output must not advertise recovery");
    });
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(command_output_artifact)]
async fn incomplete_remint_cleanup_survives_cancellation_while_registry_is_locked() {
    let temp = tempfile::tempdir().expect("artifact home");
    let body = b"source remains independently recoverable\n";
    let source = create_canonical_output_artifact(
        temp.path(),
        "source",
        &CanonicalToolResult::bytes(body.to_vec()),
    )
    .await;
    assert!(source.complete);
    let id = source.artifact_id().expect("source ID");
    let digest = format!("{:x}", Sha256::digest(body));
    let directory = temp.path().join("tool-output/target");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("target directory");
    // An existing protected sparse artifact legitimately exhausts the target's
    // retention budget without allocating or writing a 256 MiB test buffer.
    let occupied_path = directory.join(format!("{}.log", ToolOutputArtifactId::new()));
    tokio::fs::File::create(&occupied_path)
        .await
        .expect("budget fixture")
        .set_len(MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD)
        .await
        .expect("occupy target budget");
    let occupied_marker = active_tool_history_protection_path(&occupied_path);
    tokio::fs::write(
        &occupied_marker,
        ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES,
    )
    .await
    .expect("protect budget fixture");
    assert_eq!(
        force_retention_reconciliation_for_test(&temp.path().join("tool-output")).await,
        RetentionModeKind::Indexed,
        "observe the external protected budget fixture through the actual filesystem reconciler"
    );
    let target_path = directory.join(format!("{id}.log"));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    set_reconciliation_barrier(&target_path, Arc::clone(&barrier));
    let operation = tokio::spawn({
        let home = temp.path().to_path_buf();
        let id = id.clone();
        let digest = digest.clone();
        async move {
            remint_tool_history_artifact_for_thread(
                &home,
                "source",
                "target",
                &id,
                body.len() as u64,
                &digest,
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(10), barrier.wait())
        .await
        .expect("normal remint commits incomplete target");
    let metadata: serde_json::Value = serde_json::from_slice(
        &tokio::fs::read(logical_metadata_path(&target_path))
            .await
            .expect("incomplete target metadata"),
    )
    .expect("metadata JSON");
    assert_eq!(metadata["complete"], false);
    assert_eq!(metadata["retained_bytes"], 0);
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let released = Arc::new(AtomicBool::new(false));
    let thread_released = Arc::clone(&released);
    let blocker = std::thread::spawn(move || {
        let _guard = lock_retention_registry();
        locked_tx.send(()).expect("registry held");
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
        thread_released.store(true, Ordering::Release);
    });
    locked_rx.recv().expect("registry locked before rollback");
    barrier.wait().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let runtime_advanced_while_locked = !released.load(Ordering::Acquire);
    let cleanup_waited = !operation.is_finished();
    // Cancelling this public remint caller must not cancel the already-admitted
    // cleanup worker waiting on the real registry before its filesystem work.
    operation.abort();
    let cancelled = operation
        .await
        .expect_err("cancel remint caller")
        .is_cancelled();
    let _ = release_tx.send(());
    blocker.join().expect("registry blocker");
    assert!(runtime_advanced_while_locked);
    assert!(cleanup_waited);
    assert!(cancelled);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut entries = tokio::fs::read_dir(&directory)
                .await
                .expect("target directory");
            let mut target_family_exists = false;
            while let Some(entry) = entries.next_entry().await.expect("target entry") {
                target_family_exists |= entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{id}."));
            }
            if !target_family_exists {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("owned cleanup removes complete target family after cancellation");
    assert_eq!(
        tokio::fs::metadata(&occupied_path)
            .await
            .expect("unrelated protected artifact retained")
            .len(),
        MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
    );
    assert_eq!(
        tokio::fs::read(&occupied_marker)
            .await
            .expect("unrelated marker retained"),
        ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES
    );
    assert_eq!(
        tokio::fs::read(
            temp.path()
                .join("tool-output/source")
                .join(format!("{id}.log"))
        )
        .await
        .expect("source retained"),
        body
    );
    tokio::fs::remove_file(&occupied_marker)
        .await
        .expect("release fixture protection");
    tokio::fs::remove_file(&occupied_path)
        .await
        .expect("release fixture budget");
    assert_eq!(
        force_retention_reconciliation_for_test(&temp.path().join("tool-output")).await,
        RetentionModeKind::Indexed,
        "reconcile externally removed fixture bytes before the ordinary retry"
    );
    let retry = remint_tool_history_artifact_for_thread(
        temp.path(),
        "source",
        "target",
        &id,
        body.len() as u64,
        &digest,
    )
    .await
    .expect("ordinary retry after cancelled cleanup succeeds");
    assert_eq!(retry, id);
    assert_eq!(
        tokio::fs::read(&target_path)
            .await
            .expect("retry target bytes"),
        body
    );
    assert_eq!(
        tokio::fs::read(active_tool_history_protection_path(&target_path))
            .await
            .expect("retry protection"),
        ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES
    );
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn remint_protection_failure_cleans_target_and_preserves_source_for_retry() {
    let temp = tempfile::tempdir().expect("artifact home");
    let body = b"protection failure must not discard the source\n";
    let source = create_canonical_output_artifact(
        temp.path(),
        "source",
        &CanonicalToolResult::bytes(body.to_vec()),
    )
    .await;
    assert!(source.complete);
    let id = source.artifact_id().expect("source artifact ID");
    let digest = format!("{:x}", Sha256::digest(body));
    let target_path = temp
        .path()
        .join("tool-output/target")
        .join(format!("{id}.log"));
    tokio::fs::create_dir_all(target_path.parent().expect("target directory"))
        .await
        .expect("target directory");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    set_reconciliation_barrier(&target_path, Arc::clone(&barrier));
    let operation = tokio::spawn({
        let home = temp.path().to_path_buf();
        let id = id.clone();
        let digest = digest.clone();
        async move {
            remint_tool_history_artifact_for_thread(
                &home,
                "source",
                "target",
                &id,
                body.len() as u64,
                &digest,
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), barrier.wait())
        .await
        .expect("normal target commit");
    assert_eq!(
        tokio::fs::read(&target_path)
            .await
            .expect("committed target"),
        body
    );
    let marker = active_tool_history_protection_path(&target_path);
    // Fault only the external filesystem after the normal target commit. The
    // production protector must detect this invalid marker and trigger cleanup.
    tokio::fs::write(&marker, b"invalid external marker")
        .await
        .expect("inject invalid marker");
    barrier.wait().await;
    let error = operation
        .await
        .expect("remint task")
        .expect_err("invalid marker must fail protection");
    assert!(
        error.contains("failed to protect reminted artifact"),
        "{error}"
    );
    assert!(error.contains("protection marker is invalid"), "{error}");
    let mut entries = tokio::fs::read_dir(target_path.parent().expect("target directory"))
        .await
        .expect("target entries");
    while let Some(entry) = entries.next_entry().await.expect("target entry") {
        assert!(
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{id}.")),
            "failed remint retained {}",
            entry.path().display()
        );
    }
    assert_eq!(
        tokio::fs::read(
            temp.path()
                .join("tool-output/source")
                .join(format!("{id}.log"))
        )
        .await
        .expect("source unaffected"),
        body
    );
    assert_eq!(
        remint_tool_history_artifact_for_thread(
            temp.path(),
            "source",
            "target",
            &id,
            body.len() as u64,
            &digest
        )
        .await
        .expect("normal retry succeeds"),
        id
    );
    assert_eq!(
        tokio::fs::read(&target_path).await.expect("retry target"),
        body
    );
    assert_eq!(
        tokio::fs::read(&marker).await.expect("retry valid marker"),
        ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES
    );
}
