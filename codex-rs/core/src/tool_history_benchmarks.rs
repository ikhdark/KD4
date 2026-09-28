//! Production-path regressions. Setting CODEX_TOOL_HISTORY_BENCH_OUTPUT_DIR also records timings.

use super::*;
use pretty_assertions::assert_eq;
use std::hint::black_box;
use std::time::Duration;
use std::time::Instant;

const OBSERVATIONS: usize = 16;

#[expect(clippy::print_stdout, reason = "emits benchmark measurements")]
pub(super) fn report(value: serde_json::Value) {
    if let Some(directory) = benchmark_output_directory() {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("{}.json", value["case"].as_str().unwrap())),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
    }
    println!("TOOL_HISTORY_BENCH {value}");
}

pub(super) fn benchmark_output_directory() -> Option<std::ffi::OsString> {
    // Windows test isolation clears Desktop's CODEX_* environment. The KD4
    // spelling is an explicit benchmark-only opt-in, not application state.
    std::env::var_os("KD4_TOOL_HISTORY_BENCH_OUTPUT_DIR")
        .or_else(|| std::env::var_os("CODEX_TOOL_HISTORY_BENCH_OUTPUT_DIR"))
        .filter(|value| !value.is_empty())
}

fn measure(mut operation: impl FnMut()) -> serde_json::Value {
    if benchmark_output_directory().is_none() {
        return serde_json::Value::Null;
    }
    for _ in 0..8 {
        operation();
    }
    let mut samples = Vec::new();
    for _ in 0..9 {
        let start = Instant::now();
        for _ in 0..32 {
            operation();
        }
        samples.push(start.elapsed().as_secs_f64() * 1_000_000.0 / 32.0);
    }
    samples.sort_by(f64::total_cmp);
    serde_json::json!({"median_us": samples[4], "max_batch_us": samples[8], "iterations": 288})
}

async fn fixture() -> (
    tempfile::TempDir,
    Arc<GitWorkspaceCache>,
    WorkspaceEvidenceIdentity,
    SourcePathChangeObservation,
) {
    let root = tempfile::tempdir().unwrap();
    let git = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(git.status.success(), "{git:?}");
    std::fs::write(root.path().join(".gitignore"), "source.txt\n").unwrap();
    std::fs::write(root.path().join("source.txt"), "original source\n").unwrap();
    std::fs::write(root.path().join("other.txt"), "unrelated original\n").unwrap();
    let cache = GitWorkspaceCache::new();
    let proof = cache
        .begin_source_path_change_observation(root.path(), &root.path().join("source.txt"), false)
        .await
        .expect("real filesystem watcher must be available");
    let revision = cache
        .workspace_evidence_identity(root.path())
        .await
        .unwrap();
    assert!(!revision.unavailable);
    assert!(cache.source_path_change_observation_is_current(&proof));
    (root, cache, revision, proof)
}

fn observed_history(
    root: &Path,
    revision: &WorkspaceEvidenceIdentity,
    proof: &SourcePathChangeObservation,
) -> (ToolHistoryState, Arc<[ResponseItem]>) {
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    for index in 0..OBSERVATIONS {
        let id = format!("bench-{index}");
        let output = text_output(
            &id,
            format!("result {index}: {}", "verified source detail\n".repeat(128)),
        );
        state.register_workspace_evidence(
            WorkspaceEvidenceObservation::from_response_item(
                Some(revision.clone()),
                &output,
                BTreeSet::from([SourceDependencyV1::new(&root.join("source.txt"), false)]),
            )
            .unwrap()
            .with_source_path_observations(vec![proof.clone()]),
        );
        items.extend([function_call(&id), output]);
    }
    (state, items.into())
}

async fn wait_for_change(cache: &GitWorkspaceCache, proof: &SourcePathChangeObservation) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while cache.source_path_change_observation_is_current(proof) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("real watcher must observe the changed input");
}

fn notices(items: &[ResponseItem]) -> Vec<(String, serde_json::Value)> {
    items
        .iter()
        .filter_map(|item| {
            let ResponseItem::Message { role, content, .. } = item else {
                return None;
            };
            if role != "developer" {
                return None;
            }
            content.iter().find_map(|content| {
                let codex_protocol::models::ContentItem::InputText { text } = content else {
                    return None;
                };
                if !text.starts_with("<workspace_evidence_invalidation>\n") {
                    return None;
                }
                let json = text.lines().find(|line| line.starts_with('{')).unwrap();
                Some((text.clone(), serde_json::from_str(json).unwrap()))
            })
        })
        .collect()
}

#[tokio::test]
async fn repeated_invalidation_notices() {
    let (root, cache, revision, proof) = fixture().await;
    let (mut state, canonical) = observed_history(root.path(), &revision, &proof);
    let initial = state.project_sampling_with_workspace_cache(
        Arc::clone(&canonical),
        Some(&revision),
        &cache,
    );
    assert_eq!(initial.items, canonical);
    std::fs::write(root.path().join("source.txt"), "changed ignored source\n").unwrap();
    wait_for_change(&cache, &proof).await;
    let unchanged_git = cache
        .workspace_evidence_identity(root.path())
        .await
        .unwrap();
    assert_eq!(
        unchanged_git, revision,
        "ignored input changes must not need a Git digest change"
    );
    let mut anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&canonical),
        projection: initial,
    };
    anchor.projection = state
        .project_continuation_with_workspace_cache(
            &anchor,
            Arc::clone(&canonical),
            Some(&revision),
            &cache,
        )
        .unwrap();
    assert_eq!(notices(&anchor.projection.items).len(), OBSERVATIONS);
    for index in 0..8 {
        std::fs::write(
            root.path().join("other.txt"),
            format!("unrelated edit {index}\n"),
        )
        .unwrap();
        cache
            .note_host_workspace_mutation_paths(root.path(), &["other.txt".to_string()])
            .await;
        let current = cache
            .workspace_evidence_identity(root.path())
            .await
            .unwrap();
        state.invalidate_source_dependencies(
            Some(&BTreeSet::from([root.path().join("other.txt")])),
            Some(&current),
        );
        let next = state
            .project_continuation_with_workspace_cache(
                &anchor,
                Arc::clone(&canonical),
                Some(&current),
                &cache,
            )
            .unwrap();
        assert!(next.items.starts_with(&anchor.projection.items));
        anchor.projection = next;
    }
    let messages = notices(&anchor.projection.items);
    assert_eq!(
        messages.len(),
        OBSERVATIONS,
        "unrelated edits must not repeat the same invalidation"
    );
    assert!(
        messages
            .iter()
            .all(|(_, value)| value["valid_for_current_workspace"] == false)
    );
    report(serde_json::json!({
        "case":"repeated_notices", "observations":OBSERVATIONS, "unrelated_edits":8,
        "notices":messages.len(),
        "approx_tokens":messages.iter().map(|(text, _)| approx_token_count(text)).sum::<usize>(),
    }));
}

#[tokio::test]
async fn compaction_watcher_propagation() {
    let (root, cache, revision, proof) = fixture().await;
    let (state, canonical) = observed_history(root.path(), &revision, &proof);
    let baseline = state.project_with_workspace_identity(Arc::clone(&canonical), Some(&revision));
    let proposed =
        state.project_with_workspace_cache(Arc::clone(&canonical), Some(&revision), &cache);
    assert_eq!(proposed.items, canonical);
    let stale_count = |items: &[ResponseItem]| {
        items
            .iter()
            .filter_map(canonical_textual_output_identity)
            .filter(|(_, text)| text.contains("\"stale_workspace_evidence\":true"))
            .count()
    };
    assert_eq!(stale_count(&baseline.items), OBSERVATIONS);
    let mut history = crate::context_manager::ContextManager::new();
    history.record_items(
        canonical.iter(),
        codex_utils_output_truncation::TruncationPolicy::Tokens(100_000),
    );
    history.set_tool_history_state(state.clone());
    let local = history.for_compaction_prompt_with_completed_tool_projection(
        &codex_protocol::openai_models::default_input_modalities(),
        Some(&revision),
        &cache,
    );
    assert_eq!(
        stale_count(&local),
        0,
        "exercise the actual local-compaction entrypoint"
    );
    let baseline_time = measure(|| {
        black_box(state.project_with_workspace_identity(Arc::clone(&canonical), Some(&revision)));
    });
    let proposed_time = measure(|| {
        black_box(state.project_with_workspace_cache(
            Arc::clone(&canonical),
            Some(&revision),
            &cache,
        ));
    });
    std::fs::write(root.path().join("other.txt"), "disjoint edit\n").unwrap();
    cache
        .note_host_workspace_mutation_paths(root.path(), &["other.txt".to_string()])
        .await;
    let disjoint = cache
        .workspace_evidence_identity(root.path())
        .await
        .unwrap();
    assert_eq!(
        state
            .project_with_workspace_cache(Arc::clone(&canonical), Some(&disjoint), &cache)
            .items,
        canonical
    );
    std::fs::write(root.path().join("source.txt"), "changed ignored input\n").unwrap();
    wait_for_change(&cache, &proof).await;
    assert_eq!(
        stale_count(
            &state
                .project_with_workspace_cache(canonical, Some(&disjoint), &cache)
                .items
        ),
        OBSERVATIONS
    );
    report(serde_json::json!({
        "case":"compaction_watcher", "observations":OBSERVATIONS,
        "baseline_false_stale":OBSERVATIONS, "prototype_false_stale":0,
        "changed_input_rejections":OBSERVATIONS,
        "baseline":baseline_time, "prototype":proposed_time,
    }));
}

#[tokio::test]
async fn compaction_recovery_ordering() {
    let home = tempfile::tempdir().unwrap();
    let output = "historical evidence that must remain exactly recoverable\n".repeat(128);
    let (tracked, exact) =
        stored_candidate(home.path(), "benchmark", "stored", output.clone()).await;
    let artifact_id = tracked.artifact_id.clone();
    let mut state = ToolHistoryState::default();
    state.register(tracked);
    let item = text_output("stored", output);
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(workspace_identity("old")),
            &item,
            BTreeSet::new(),
        )
        .unwrap(),
    );
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call("stored"), item]);
    state.mark_consumed(
        &canonical,
        ModelGenerationId {
            turn_id: "benchmark".into(),
            ordinal: 1,
        },
    );
    let changed = workspace_identity("changed");
    let projection = state.project_with_workspace_identity(Arc::clone(&canonical), Some(&changed));
    assert!(
        state
            .artifact_pin_payload_for_items(&projection.items)
            .is_none()
    );
    let pins = state.artifact_pin_payload_for_items(&canonical).unwrap();
    assert!(approx_token_count(&pins) <= COMPACTION_ARTIFACT_PIN_TOKEN_BUDGET);
    let baseline_time = measure(|| {
        black_box(state.artifact_pin_payload_for_items(&projection.items));
    });
    let proposed_time = measure(|| {
        black_box(state.artifact_pin_payload_for_items(&canonical));
    });
    let mut lost = state.clone();
    lost.retain_for_history(&[text_output(
        "summary",
        "summary without copied handles".into(),
    )]);
    assert!(lost.artifact_references().is_empty());
    state.retain_for_history(&[text_output("summary", pins.clone())]);
    assert_eq!(state.artifact_references().len(), 1);
    persist_tool_history_state(home.path(), "benchmark", &state)
        .await
        .unwrap();
    let resumed =
        expect_loaded_tool_history(load_tool_history_state(home.path(), "benchmark").await);
    assert_eq!(resumed.artifact_references().len(), 1);
    let recovered = crate::tools::command_output_artifact::read_complete_canonical_snapshot(
        home.path(),
        "benchmark",
        &artifact_id,
        exact.len(),
    )
    .await
    .unwrap();
    assert_eq!(recovered, exact);
    let recovery: Arc<[ResponseItem]> = Arc::from([
        named_function_call_with_arguments(
            "recover",
            "read_tool_output",
            serde_json::json!({"artifact_id":artifact_id}),
        ),
        text_output("recover", String::from_utf8(recovered).unwrap()),
    ]);
    let projected_recovery = resumed.project_with_workspace_identity(recovery, Some(&changed));
    let (_, text) = canonical_textual_output_identity(&projected_recovery.items[1]).unwrap();
    let notice: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(notice["valid_for_current_workspace"], false);
    assert_eq!(
        notice["historical_output"],
        String::from_utf8(exact.clone()).unwrap()
    );
    report(serde_json::json!({
        "case":"recovery_ordering", "baseline_handles":0, "prototype_handles":1,
        "recovered_bytes":exact.len(), "sidecar_approx_tokens":approx_token_count(&pins),
        "freshness_after_resume":false, "baseline":baseline_time, "prototype":proposed_time,
    }));
}

#[tokio::test]
async fn search_input_dependencies() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/input.txt"), "present\n").unwrap();
    let mut cases = Vec::new();
    for (name, options, input, before, after) in [
        (
            "pattern_file",
            vec!["-f", "patterns.txt"],
            "patterns.txt",
            "absent\n",
            "present\n",
        ),
        (
            "explicit_ignore",
            vec!["--ignore-file", "rules.txt", "present"],
            "rules.txt",
            "*.txt\n",
            "# include\n",
        ),
        (
            "ancestor_ignore",
            vec!["present"],
            ".ignore",
            "*.txt\n",
            "# include\n",
        ),
    ] {
        std::fs::write(root.path().join(input), before).unwrap();
        let command = std::iter::once("rg")
            .chain(options)
            .chain(std::iter::once("src"))
            .map(str::to_string)
            .collect::<Vec<_>>();
        let arguments = serde_json::json!({"program":"rg", "args":&command[1..]});
        let payload = ToolPayload::Function {
            arguments: arguments.to_string(),
        };
        let baseline =
            classify_workspace_tool_call("exec_command", &payload, root.path()).source_dependencies;
        let classify = || {
            crate::tools::handlers::command_search::classify_rg_search_narrowing(
                &command,
                None,
                root.path(),
                root.path(),
            )
            .unwrap()
            .unwrap()
        };
        let proposed = classify()
            .state_paths
            .iter()
            .map(|path| SourceDependencyV1::new(path, path.is_dir()))
            .collect::<BTreeSet<_>>();
        let source_input = SourceDependencyV1::new(&root.path().join(input), false);
        assert!(baseline.contains(&source_input));
        assert!(proposed.contains(&source_input));
        let run = || {
            std::process::Command::new("rg")
                .args(&command[1..])
                .current_dir(root.path())
                .output()
                .unwrap()
        };
        let miss = run();
        assert_eq!(miss.status.code(), Some(1), "{miss:?}");
        assert!(miss.stdout.is_empty());
        let item = text_output("search", "Exit code: 1\nno matches".into());
        let canonical: Arc<[ResponseItem]> = Arc::from([
            named_function_call_with_arguments("search", "exec_command", arguments),
            item.clone(),
        ]);
        let make_state = |dependencies| {
            let mut state = ToolHistoryState::default();
            state.register_workspace_evidence(
                WorkspaceEvidenceObservation::from_response_item(
                    Some(workspace_identity("before")),
                    &item,
                    dependencies,
                )
                .unwrap(),
            );
            state
        };
        let mut old_state = make_state(baseline.clone());
        let mut new_state = make_state(proposed.clone());
        let disjoint = workspace_identity("disjoint");
        for state in [&mut old_state, &mut new_state] {
            state.invalidate_source_dependencies(
                Some(&BTreeSet::from([root.path().join("unrelated.md")])),
                Some(&disjoint),
            );
            assert_eq!(
                state
                    .project_with_workspace_identity(Arc::clone(&canonical), Some(&disjoint))
                    .items,
                canonical
            );
        }
        std::fs::write(root.path().join(input), after).unwrap();
        let actual = run();
        assert_eq!(actual.status.code(), Some(0), "{actual:?}");
        assert!(
            String::from_utf8(actual.stdout)
                .unwrap()
                .contains("present")
        );
        let current = workspace_identity("after");
        for state in [&mut old_state, &mut new_state] {
            state.invalidate_source_dependencies(
                Some(&BTreeSet::from([root.path().join(input)])),
                Some(&current),
            );
        }
        let repaired =
            old_state.project_with_workspace_identity(Arc::clone(&canonical), Some(&current));
        let (_, text) = canonical_textual_output_identity(&repaired.items[1]).unwrap();
        assert!(text.contains("\"stale_workspace_evidence\":true"));
        let updated =
            new_state.project_with_workspace_identity(Arc::clone(&canonical), Some(&current));
        let (_, text) = canonical_textual_output_identity(&updated.items[1]).unwrap();
        assert!(text.contains("\"stale_workspace_evidence\":true"));
        let baseline_time = measure(|| {
            black_box(classify_workspace_tool_call(
                "exec_command",
                &payload,
                root.path(),
            ));
        });
        let proposed_time = measure(|| {
            black_box(classify());
        });
        cases.push(
            serde_json::json!({"case":name, "production_dependencies":baseline.len(),
            "reference_dependencies":proposed.len(), "production_stale_accepted":false,
            "reference_stale_accepted":false, "unrelated_edit_preserved":true,
            "production":baseline_time, "reference":proposed_time}),
        );
    }
    report(serde_json::json!({"case":"search_inputs", "cases":cases,
        "timing_scope":"production shared-parser history classifier vs full search-state classifier"}));
}

#[tokio::test]
async fn invalidation_updates_preserve_nested_and_authentication_changes() {
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let revision = workspace_identity("captured");
    let parent = text_output("parent", "combined output".into());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call("parent"), parent.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(revision.clone()),
            &parent,
            BTreeSet::new(),
        )
        .unwrap(),
    );
    for id in ["a", "b"] {
        state.register_workspace_evidence(
            WorkspaceEvidenceObservation::from_response_item(
                Some(revision.clone()),
                &text_output(id, id.into()),
                BTreeSet::from([SourceDependencyV1::new(
                    &PathBuf::from("/repo").join(id),
                    false,
                )]),
            )
            .unwrap(),
        );
        assert!(
            ToolHistoryMutation::RegisterCodeModeNestedEvidence {
                parent_call_id: "parent".into(),
                call_id: id.into(),
                output: id.into(),
            }
            .apply(&mut state)
        );
    }
    let mut anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&canonical),
        projection: state.project_sampling_with_workspace_cache(
            Arc::clone(&canonical),
            Some(&revision),
            &cache,
        ),
    };
    for (index, id) in ["a", "b"].into_iter().enumerate() {
        state.invalidate_source_dependencies(
            Some(&BTreeSet::from([PathBuf::from("/repo").join(id)])),
            Some(&revision),
        );
        let next = state
            .project_continuation_with_workspace_cache(
                &anchor,
                Arc::clone(&canonical),
                Some(&revision),
                &cache,
            )
            .unwrap();
        assert!(next.items.starts_with(&anchor.projection.items));
        let messages = notices(&next.items);
        assert_eq!(messages.len(), index + 1);
        assert_eq!(
            messages.last().unwrap().1["current_nested_results"],
            if index == 0 {
                serde_json::json!([{"call_id":"b"}])
            } else {
                serde_json::Value::Null
            }
        );
        anchor.projection = next;
    }
    let missing = ToolHistoryState::default().project_sampling_with_workspace_cache(
        Arc::clone(&canonical),
        Some(&revision),
        &cache,
    );
    let mut mismatched = ToolHistoryState::default();
    mismatched.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(revision.clone()),
            &text_output("parent", "different authenticated output".into()),
            BTreeSet::new(),
        )
        .unwrap(),
    );
    let anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&canonical),
        projection: missing,
    };
    let next = mismatched
        .project_continuation_with_workspace_cache(&anchor, canonical, Some(&revision), &cache)
        .unwrap();
    let messages = notices(&next.items);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].1["reason_code"], "missing_observation");
    assert_eq!(messages[1].1["reason_code"], "output_mismatch");
}
