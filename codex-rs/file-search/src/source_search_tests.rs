use super::*;
use pretty_assertions::assert_eq;
use sha2::Digest;
use std::fs;

fn search_options(repo_root: &Path, query: &str) -> SourceSearchOptions {
    SourceSearchOptions::new(repo_root.to_path_buf(), query.to_string())
}

#[test]
fn read_preserves_crlf_and_builds_four_deterministic_chunks() {
    let content = (1..=126)
        .map(|line| format!("line {line}\r\n"))
        .collect::<String>();

    let output = read_file_span_from_bytes(
        "src/fixture.rs".to_string(),
        content.as_bytes().to_vec(),
        1,
        126,
    )
    .expect("read exact source span");

    assert_eq!(output.exact_content, content);
    assert_eq!(output.requested_bytes, content.len());
    assert_eq!(
        output.requested_content_sha256,
        format!("{:x}", Sha256::digest(content.as_bytes()))
    );
    assert_eq!(output.chunks.len(), 4);
    assert_eq!(
        output
            .chunks
            .iter()
            .map(|chunk| (chunk.start_line, chunk.end_line))
            .collect::<Vec<_>>(),
        vec![(1, 40), (41, 80), (81, 120), (121, 126)]
    );
    assert!(
        output
            .chunks
            .iter()
            .all(|chunk| chunk.end_line - chunk.start_line < 40 && chunk.exact_bytes <= 8 * 1024)
    );
    let reconstructed = output
        .chunks
        .iter()
        .map(|chunk| &output.exact_content[chunk.byte_start..chunk.byte_end])
        .collect::<String>();
    assert_eq!(reconstructed, content);
    assert!(output.chunks.iter().all(|chunk| {
        chunk.id
            == format!(
                "src:{}:L{}-L{}",
                &output.requested_content_sha256[..16],
                chunk.start_line,
                chunk.end_line
            )
    }));
}

#[test]
fn coverage_trim_rebuilds_exact_content_and_chunks_for_disjoint_ranges() {
    let content = (1..=8)
        .map(|line| format!("line {line}\r\n"))
        .collect::<String>();
    let mut output =
        read_file_span_from_bytes("src/fixture.rs".to_string(), content.into_bytes(), 1, 8)
            .expect("read exact source span");

    retain_read_file_span_intervals(&mut output, &[(2, 2), (5, 6)]);

    assert_eq!(output.exact_content, "line 2\r\nline 5\r\nline 6\r\n");
    assert_eq!(
        output
            .lines
            .iter()
            .map(|line| line.line_number)
            .collect::<Vec<_>>(),
        vec![2, 5, 6]
    );
    assert_eq!(
        output
            .chunks
            .iter()
            .map(|chunk| (chunk.start_line, chunk.end_line))
            .collect::<Vec<_>>(),
        vec![(2, 2), (5, 6)]
    );
    let reconstructed = output
        .chunks
        .iter()
        .map(|chunk| &output.exact_content[chunk.byte_start..chunk.byte_end])
        .collect::<String>();
    assert_eq!(reconstructed, output.exact_content);
    assert_eq!(
        output.requested_content_sha256,
        format!("{:x}", Sha256::digest(output.exact_content.as_bytes()))
    );
}

#[test]
fn one_oversized_source_line_gets_its_own_exact_chunk() {
    let oversized = format!("{}\r\nsmall\r\n", "x".repeat(9 * 1024));

    let output = read_file_span_from_bytes(
        "src/oversized.rs".to_string(),
        oversized.as_bytes().to_vec(),
        1,
        2,
    )
    .expect("read source containing oversized line");

    assert_eq!(output.exact_content, oversized);
    assert_eq!(output.chunks.len(), 2);
    assert_eq!(
        (output.chunks[0].start_line, output.chunks[0].end_line),
        (1, 1)
    );
    assert!(output.chunks[0].exact_bytes > 8 * 1024);
    assert_eq!(
        (output.chunks[1].start_line, output.chunks[1].end_line),
        (2, 2)
    );
}

#[test]
fn search_is_deterministic_and_returns_one_based_context_spans() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(repo.path().join("codex-rs/core/src")).expect("mkdir");
    fs::write(
        repo.path().join("codex-rs/core/src/b.rs"),
        "before\nneedle b\nafter\n",
    )
    .expect("write b");
    fs::write(
        repo.path().join("codex-rs/core/src/a.rs"),
        "before\nneedle a\nafter\n",
    )
    .expect("write a");
    let mut options = search_options(repo.path(), "needle");
    options.context_lines = 1;

    let output = search_source(options).expect("search");

    assert_eq!(
        output
            .matches
            .iter()
            .map(|source_match| source_match.path.as_str())
            .collect::<Vec<_>>(),
        vec!["codex-rs/core/src/a.rs", "codex-rs/core/src/b.rs"]
    );
    assert_eq!(output.matches[0].line_number, 2);
    assert_eq!(output.matches[0].start_line, 1);
    assert_eq!(output.matches[0].end_line, 3);
    assert_eq!(
        output.matches[0]
            .lines
            .iter()
            .map(|line| line.line_number)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(output.matches[0].source_map_route, Some("core".to_string()));
}

#[test]
fn all_twenty_two_match_identities_survive_context_projection() {
    let repo = tempfile::tempdir().expect("tempdir");
    let padding = "p".repeat(SOURCE_SEARCH_MAX_LINE_BYTES);
    let mut lines = Vec::new();
    for index in 0..22 {
        lines.extend((0..5).map(|_| padding.clone()));
        lines.push(format!("needle match {index} {padding}"));
        lines.extend((0..5).map(|_| padding.clone()));
    }
    fs::write(repo.path().join("large.rs"), lines.join("\n")).expect("write fixture");
    let mut options = search_options(repo.path(), "needle");
    options.max_matches = 22;
    options.context_lines = SOURCE_SEARCH_MAX_CONTEXT_LINES;

    let output = search_source(options).expect("search");

    assert_eq!(output.coverage.total_matches, 22);
    assert_eq!(output.coverage.indexed_matches, 22);
    assert_eq!(output.matches.len(), 22);
    assert!(output.coverage.index_complete);
    assert!(!output.coverage.result_cap_reached);
    assert!(!output.coverage.context_complete);
    assert!(output.coverage.omitted_contexts > 0);
    assert!(output.matches.iter().all(|source_match| {
        !source_match.id.is_empty()
            && !source_match.file_id.is_empty()
            && !source_match.source_revision.is_empty()
            && source_match.matched_content.contains("needle")
    }));
    assert!(
        output
            .matches
            .windows(2)
            .all(|matches| matches[0].file_id == matches[1].file_id)
    );
    assert_eq!(
        output
            .matches
            .iter()
            .map(|source_match| source_match.id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        22,
    );
}

#[test]
fn fixed_string_search_treats_punctuation_literally() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("source.rs"), "alpha.beta\nalphaXbeta\n").expect("write");

    let output = search_source(search_options(repo.path(), "alpha.beta")).expect("search");

    assert_eq!(output.coverage.total_matches, 1);
    assert_eq!(output.matches[0].line_number, 1);
}

#[test]
fn unique_complete_search_hydrates_from_the_observed_bytes() {
    let repo = tempfile::tempdir().expect("tempdir");
    let source = (1..=80)
        .map(|line| {
            if line == 40 {
                "fn unique_needle() {}".to_string()
            } else {
                format!("// line {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(repo.path().join("source.rs"), &source).expect("write");

    let output = search_source(search_options(repo.path(), "unique_needle")).expect("search");

    assert_eq!(
        output.hydration_status,
        SourceSearchHydrationStatus::HydratedDeterministicWindow
    );
    let hydrated = output.hydrated_span.expect("hydrated unique span");
    assert_eq!(hydrated.observation.path, "source.rs");
    assert_eq!(hydrated.observation.start_line, Some(20));
    assert!(
        hydrated
            .observation
            .lines
            .iter()
            .any(|line| line.text.contains("unique_needle"))
    );
    assert_eq!(
        hydrated.content_hash,
        format!("{:x}", sha2::Sha256::digest(source.as_bytes()))
    );
    assert!(output.hydration_packet.is_none());
}

#[test]
fn unique_search_prefers_an_existing_authoritative_definition_span() {
    let repo = tempfile::tempdir().expect("tempdir");
    let source = (1..=80)
        .map(|line| {
            if line == 40 {
                "let unique_needle = true;".to_string()
            } else {
                format!("// line {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(repo.path().join("source.rs"), &source).expect("write");
    let mut options = search_options(repo.path(), "unique_needle");
    options.hydration_candidates = vec![SourceSearchHydrationCandidate {
        path: "source.rs".to_string(),
        start_line: 35,
        end_line: 45,
        kind: SourceSearchHydrationCandidateKind::AuthoritativeDefinition,
    }];

    let output = search_source(options).expect("search");

    assert_eq!(
        output.hydration_status,
        SourceSearchHydrationStatus::HydratedAuthoritativeDefinition
    );
    let hydrated = output.hydrated_span.expect("hydrated definition");
    assert_eq!(hydrated.observation.start_line, Some(35));
    assert_eq!(hydrated.observation.end_line, Some(45));
    assert_eq!(
        hydrated.content_hash,
        format!("{:x}", sha2::Sha256::digest(source.as_bytes()))
    );
}

#[test]
fn complete_multi_match_search_hydrates_a_bounded_exact_packet() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("a.rs"), "before a\nneedle a\nafter a\n").expect("write a");
    fs::write(repo.path().join("b.rs"), "before b\nneedle b\nafter b\n").expect("write b");

    let output = search_source(search_options(repo.path(), "needle")).expect("search");

    assert_eq!(
        output.hydration_status,
        SourceSearchHydrationStatus::HydratedBoundedPacket
    );
    assert!(output.hydrated_span.is_none());
    let packet = output.hydration_packet.expect("multi-match packet");
    assert_eq!(packet.schema_version, 1);
    assert_eq!(packet.spans.len(), 2);
    assert!(packet.issues.is_empty());
    assert!(packet.exact_content_bytes <= SOURCE_SEARCH_HYDRATION_MAX_BYTES);
    assert_eq!(packet.spans[0].path, "a.rs");
    assert_eq!(packet.spans[1].path, "b.rs");
    assert_eq!(
        packet.spans[0].span_content_hash,
        format!(
            "{:x}",
            Sha256::digest(packet.spans[0].exact_content.as_bytes())
        )
    );
    assert!(packet.spans.iter().all(|span| {
        output.matches.iter().any(|matched| {
            span.match_ids.contains(&matched.id)
                && span.path == matched.path
                && span.file_content_hash == matched.source_revision
                && span.start_line <= matched.line_number
                && span.end_line >= matched.line_number
        })
    }));
}

#[test]
fn multi_match_hydration_deduplicates_one_authoritative_span() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(
        repo.path().join("source.rs"),
        "fn owner() {\n    let needle = 1;\n    let other_needle = 2;\n}\n",
    )
    .expect("write");
    let mut options = search_options(repo.path(), "needle");
    options.hydration_candidates = vec![SourceSearchHydrationCandidate {
        path: "source.rs".to_string(),
        start_line: 1,
        end_line: 4,
        kind: SourceSearchHydrationCandidateKind::AuthoritativeDefinition,
    }];

    let output = search_source(options).expect("search");
    let packet = output.hydration_packet.expect("multi-match packet");

    assert_eq!(packet.spans.len(), 1);
    assert_eq!(packet.spans[0].match_ids.len(), 2);
    assert_eq!(
        packet.spans[0].selection,
        SourceSearchHydrationSelection::AuthoritativeDefinition
    );
    assert_eq!(
        packet.spans[0].exact_content,
        fs::read_to_string(repo.path().join("source.rs")).expect("read")
    );
}

#[test]
fn ambiguous_multi_match_candidates_are_reported_without_fallback() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("source.rs"), "needle one\nneedle two\n").expect("write");
    for (kind, expected_reason) in [
        (
            SourceSearchHydrationCandidateKind::AuthoritativeDefinition,
            SourceSearchHydrationIssueReason::AmbiguousAuthoritativeCandidate,
        ),
        (
            SourceSearchHydrationCandidateKind::StructuredContext,
            SourceSearchHydrationIssueReason::AmbiguousStructuredCandidate,
        ),
    ] {
        let mut options = search_options(repo.path(), "needle");
        options.hydration_candidates = vec![
            SourceSearchHydrationCandidate {
                path: "source.rs".to_string(),
                start_line: 1,
                end_line: 2,
                kind,
            },
            SourceSearchHydrationCandidate {
                path: "source.rs".to_string(),
                start_line: 1,
                end_line: 3,
                kind,
            },
        ];

        let output = search_source(options).expect("search");
        let packet = output.hydration_packet.expect("partial packet");

        assert_eq!(
            output.hydration_status,
            SourceSearchHydrationStatus::PartiallyHydratedBoundedPacket
        );
        assert!(packet.spans.is_empty());
        assert_eq!(packet.issues.len(), 2);
        assert!(
            packet
                .issues
                .iter()
                .all(|issue| issue.reason == expected_reason)
        );
    }
}

#[test]
fn multi_match_packet_caps_content_and_accounts_for_every_match() {
    let repo = tempfile::tempdir().expect("tempdir");
    for index in 0..12 {
        fs::write(
            repo.path().join(format!("source_{index:02}.rs")),
            format!("needle {}\n", "x".repeat(700)),
        )
        .expect("write");
    }

    let output = search_source(search_options(repo.path(), "needle")).expect("search");
    let packet = output.hydration_packet.expect("partial packet");
    let represented = packet
        .spans
        .iter()
        .flat_map(|span| span.match_ids.iter())
        .chain(packet.issues.iter().map(|issue| &issue.match_id))
        .collect::<Vec<_>>();

    assert_eq!(
        output.hydration_status,
        SourceSearchHydrationStatus::PartiallyHydratedBoundedPacket
    );
    assert!(packet.exact_content_bytes <= SOURCE_SEARCH_HYDRATION_MAX_BYTES);
    assert!(packet.spans.len() <= SOURCE_SEARCH_HYDRATION_MAX_SPANS);
    assert_eq!(represented.len(), output.matches.len());
    assert!(
        output
            .matches
            .iter()
            .all(|matched| represented.iter().any(|id| id.as_str() == matched.id))
    );
}

#[test]
fn multi_match_packet_caps_span_count_and_reports_each_omission() {
    let repo = tempfile::tempdir().expect("tempdir");
    for index in 0..12 {
        fs::write(
            repo.path().join(format!("source_{index:02}.rs")),
            "needle\n",
        )
        .expect("write");
    }

    let output = search_source(search_options(repo.path(), "needle")).expect("search");
    let packet = output.hydration_packet.expect("partial packet");

    assert_eq!(packet.spans.len(), SOURCE_SEARCH_HYDRATION_MAX_SPANS);
    assert_eq!(
        packet
            .issues
            .iter()
            .filter(|issue| issue.reason == SourceSearchHydrationIssueReason::SpanCap)
            .count(),
        4
    );
}

#[test]
fn unavailable_multi_match_observations_are_reported_explicitly() {
    let options = SourceSearchOptions::new(PathBuf::new(), "needle".to_string());
    let mut accumulator = SourceSearchAccumulator::new(&options).expect("accumulator");
    accumulator.add_file_bytes(Path::new("a.rs"), b"needle a\n".to_vec());
    accumulator.add_file_bytes(Path::new("b.rs"), b"needle b\n".to_vec());
    accumulator.hydration_observations.clear();

    let output = accumulator.finish(vec![".".to_string()]);
    let packet = output.hydration_packet.expect("partial packet");

    assert!(packet.spans.is_empty());
    assert_eq!(packet.issues.len(), 2);
    assert!(
        packet.issues.iter().all(|issue| {
            issue.reason == SourceSearchHydrationIssueReason::ObservationUnavailable
        })
    );
}

#[test]
fn multi_match_packet_identity_is_independent_of_scan_order() {
    let options = SourceSearchOptions::new(PathBuf::new(), "needle".to_string());
    let mut first = SourceSearchAccumulator::new(&options).expect("first accumulator");
    first.add_file_bytes(Path::new("b.rs"), b"needle b\n".to_vec());
    first.add_file_bytes(Path::new("a.rs"), b"needle a\n".to_vec());
    let first = first.finish(vec![".".to_string()]);

    let mut second = SourceSearchAccumulator::new(&options).expect("second accumulator");
    second.add_file_bytes(Path::new("a.rs"), b"needle a\n".to_vec());
    second.add_file_bytes(Path::new("b.rs"), b"needle b\n".to_vec());
    let second = second.finish(vec![".".to_string()]);

    assert_eq!(
        first
            .hydration_packet
            .as_ref()
            .expect("first packet")
            .observation_set_id,
        second
            .hydration_packet
            .as_ref()
            .expect("second packet")
            .observation_set_id
    );
    assert_eq!(first.hydration_packet, second.hydration_packet);
}

#[test]
fn incomplete_match_index_rejects_multi_match_hydration() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("source.rs"), "needle one\nneedle two\n").expect("write");
    let mut options = search_options(repo.path(), "needle");
    options.max_matches = 1;

    let output = search_source(options).expect("search");

    assert!(output.coverage_complete);
    assert!(!output.coverage.index_complete);
    assert_eq!(
        output.hydration_status,
        SourceSearchHydrationStatus::SkippedIndexIncomplete
    );
    assert!(output.hydration_packet.is_none());
}

#[test]
fn source_search_output_deserializes_without_hydration_packet() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("source.rs"), "needle\n").expect("write");
    let output = search_source(search_options(repo.path(), "needle")).expect("search");
    let mut value = serde_json::to_value(output).expect("serialize output");
    value
        .as_object_mut()
        .expect("object output")
        .remove("hydration_packet");

    let decoded: SourceSearchOutput =
        serde_json::from_value(value).expect("deserialize old output");

    assert!(decoded.hydration_packet.is_none());
    assert!(decoded.hydrated_span.is_some());
}

#[test]
fn incomplete_search_does_not_hydrate() {
    let repo = tempfile::tempdir().expect("tempdir");

    let options = search_options(repo.path(), "first");
    let mut accumulator = SourceSearchAccumulator::new(&options).expect("accumulator");
    assert!(accumulator.consider_file(Path::new("first.rs"), 6));
    accumulator.add_file_bytes(Path::new("first.rs"), b"first\n".to_vec());
    accumulator.mark_filesystem_error();
    let incomplete = accumulator.finish(vec![".".to_string()]);
    assert_eq!(
        incomplete.hydration_status,
        SourceSearchHydrationStatus::SkippedCoverageIncomplete
    );
    assert!(incomplete.hydrated_span.is_none());
    assert!(incomplete.hydration_packet.is_none());
}

#[test]
fn unique_hydration_can_be_disabled_without_changing_search_results() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("source.rs"), "needle\n").expect("write");
    let mut options = search_options(repo.path(), "needle");
    options.hydrate_selected_span = false;

    let output = search_source(options).expect("search");

    assert_eq!(output.coverage.total_matches, 1);
    assert_eq!(
        output.hydration_status,
        SourceSearchHydrationStatus::Disabled
    );
    assert!(output.hydrated_span.is_none());
    assert!(output.hydration_packet.is_none());
}

#[test]
fn case_insensitive_search_matches_unicode_case_pairs() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("source.rs"), "before\nÉCOLE\nafter\n").expect("write");

    let output = search_source(search_options(repo.path(), "école")).expect("search");

    assert_eq!(output.coverage.total_matches, 1);
    assert_eq!(output.matches[0].line_number, 2);
}

#[test]
fn case_insensitive_search_handles_sigma_and_sharp_s_folds() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(
        repo.path().join("unicode.rs"),
        "const GREEK: &str = \"ΟΣ\";\nconst GERMAN: &str = \"straße\";\n",
    )
    .expect("write");

    let sigma = search_source(search_options(repo.path(), "ος")).expect("sigma search");
    assert_eq!(sigma.coverage.total_matches, 1);

    let sharp_s = search_source(search_options(repo.path(), "STRASSE")).expect("sharp-s search");
    assert_eq!(sharp_s.coverage.total_matches, 1);
}

#[test]
fn case_insensitive_search_uses_complete_unicode_default_folding() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(
        repo.path().join("unicode_folds.rs"),
        "const LONG_S: &str = \"ſource\";\nconst LIGATURE: &str = \"oﬃce\";\n",
    )
    .expect("write");

    let long_s = search_source(search_options(repo.path(), "SOURCE")).expect("long-s search");
    assert_eq!(long_s.coverage.total_matches, 1);

    let ligature = search_source(search_options(repo.path(), "OFFICE")).expect("ligature search");
    assert_eq!(ligature.coverage.total_matches, 1);
}

#[test]
fn search_reports_match_cap_without_stopping_bounded_scan() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("a.rs"), "needle one\nneedle two\n").expect("write");
    let mut options = search_options(repo.path(), "needle");
    options.max_matches = 1;

    let output = search_source(options).expect("search");

    assert_eq!(output.coverage.total_matches, 2);
    assert_eq!(output.matches.len(), 1);
    assert_eq!(
        output.truncated_reason,
        Some(SourceTruncatedReason::MaxMatches)
    );
}

#[test]
fn search_result_text_never_exceeds_result_budget() {
    let repo = tempfile::tempdir().expect("tempdir");
    let line = format!("needle {}\n", "\\\"".repeat(SOURCE_SEARCH_MAX_LINE_BYTES));
    fs::write(repo.path().join("many.rs"), line.repeat(180)).expect("write");
    let mut options = search_options(repo.path(), "needle");
    options.max_matches = SOURCE_SEARCH_MAX_MATCHES;

    let output = search_source(options).expect("search");

    assert!(output.coverage.result_bytes <= SOURCE_SEARCH_MAX_RESULT_BYTES);
    assert_eq!(
        output.coverage.result_bytes,
        serde_json::to_vec_pretty(&output)
            .expect("serialize source search output")
            .len()
            + 1
    );
    assert_eq!(
        output.truncated_reason,
        Some(SourceTruncatedReason::MaxResultBytes)
    );
    assert!(
        output
            .matches
            .iter()
            .flat_map(|source_match| &source_match.lines)
            .all(|line| line.text.len() <= SOURCE_SEARCH_MAX_LINE_BYTES)
    );
}

#[test]
fn walk_errors_mark_coverage_incomplete_without_stopping_the_scan() {
    let repo = tempfile::tempdir().expect("tempdir");
    let options = search_options(repo.path(), "needle");
    let mut accumulator = SourceSearchAccumulator::new(&options).expect("accumulator");

    assert!(accumulator.consider_file(Path::new("a.rs"), 7));
    accumulator.add_file_bytes(Path::new("a.rs"), b"needle\n".to_vec());
    let walk_error = Result::<(), std::io::Error>::Err(std::io::Error::other("walk failed"));
    assert!(recover_walk_entry(walk_error, &mut accumulator).is_none());
    assert!(!accumulator.should_stop());
    assert!(accumulator.consider_file(Path::new("b.rs"), 7));
    accumulator.add_file_bytes(Path::new("b.rs"), b"needle\n".to_vec());

    let output = accumulator.finish(vec![".".to_string()]);

    assert_eq!(output.coverage.filesystem_errors, 1);
    assert_eq!(output.coverage.matches_returned, 2);
    assert!(output.truncated);
    assert_eq!(
        output.truncated_reason,
        Some(SourceTruncatedReason::FilesystemErrors)
    );
}

#[test]
fn per_file_scan_errors_preserve_partial_results_and_continue() {
    let repo = tempfile::tempdir().expect("tempdir");
    let options = search_options(repo.path(), "needle");
    let mut accumulator = SourceSearchAccumulator::new(&options).expect("accumulator");

    assert!(accumulator.consider_file(Path::new("before.rs"), 7));
    accumulator.add_file_bytes(Path::new("before.rs"), b"needle\n".to_vec());
    recover_scan_result(
        Err(anyhow::anyhow!("file disappeared after enumeration")),
        &mut accumulator,
    );
    assert!(accumulator.consider_file(Path::new("after.rs"), 7));
    accumulator.add_file_bytes(Path::new("after.rs"), b"needle\n".to_vec());

    let output = accumulator.finish(vec![".".to_string()]);

    assert_eq!(output.coverage.filesystem_errors, 1);
    assert_eq!(output.coverage.matches_returned, 2);
    assert_eq!(
        output.truncated_reason,
        Some(SourceTruncatedReason::FilesystemErrors)
    );
}

#[test]
fn search_rejects_roots_outside_repo_and_dedupes_nested_roots() {
    let parent = tempfile::tempdir().expect("tempdir");
    let repo = parent.path().join("repo");
    let source = repo.join("src");
    fs::create_dir_all(&source).expect("mkdir");
    fs::write(source.join("lib.rs"), "needle\n").expect("write source");
    let outside = parent.path().join("outside.rs");
    fs::write(&outside, "needle\n").expect("write outside");

    let mut confined = search_options(&repo, "needle");
    confined.roots = vec![outside];
    let error = search_source(confined).expect_err("outside root rejected");
    assert!(error.to_string().contains("outside repository root"));

    let mut nested = search_options(&repo, "needle");
    nested.roots = vec![PathBuf::from("src"), PathBuf::from(".")];
    let output = search_source(nested).expect("nested roots");
    assert_eq!(output.roots, vec!["."]);
    assert_eq!(output.coverage.total_matches, 1);
}

#[test]
fn generated_vendor_and_lock_paths_are_excluded_by_default() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(repo.path().join("target")).expect("target");
    fs::create_dir_all(repo.path().join("vendor")).expect("vendor");
    fs::write(repo.path().join("source.rs"), "needle source\n").expect("source");
    fs::write(
        repo.path().join("target/generated.rs"),
        "needle generated\n",
    )
    .expect("generated");
    fs::write(repo.path().join("vendor/dependency.rs"), "needle vendor\n").expect("vendor");
    fs::write(repo.path().join("Cargo.lock"), "needle lock\n").expect("lock");

    let output = search_source(search_options(repo.path(), "needle")).expect("search");

    assert_eq!(
        output
            .matches
            .iter()
            .map(|source_match| source_match.path.as_str())
            .collect::<Vec<_>>(),
        vec!["source.rs"]
    );
}

#[test]
fn explicit_generated_and_vendor_roots_still_use_repository_relative_exclusions() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(repo.path().join("target")).expect("target");
    fs::create_dir_all(repo.path().join("vendor")).expect("vendor");
    fs::write(
        repo.path().join("target/generated.rs"),
        "needle generated\n",
    )
    .expect("generated");
    fs::write(repo.path().join("vendor/dependency.rs"), "needle vendor\n").expect("vendor");

    let mut excluded = search_options(repo.path(), "needle");
    excluded.roots = vec![PathBuf::from("target"), PathBuf::from("vendor")];
    let output = search_source(excluded).expect("excluded search");
    assert!(output.matches.is_empty());

    let mut included = search_options(repo.path(), "needle");
    included.roots = vec![PathBuf::from("target"), PathBuf::from("vendor")];
    included.include_generated = true;
    included.include_vendor = true;
    let output = search_source(included).expect("included search");
    assert_eq!(
        output
            .matches
            .iter()
            .map(|source_match| source_match.path.as_str())
            .collect::<Vec<_>>(),
        vec!["target/generated.rs", "vendor/dependency.rs"]
    );
}

#[test]
fn common_source_language_extensions_are_scanned() {
    let repo = tempfile::tempdir().expect("tempdir");
    let source_paths = [
        "main.go",
        "main.c",
        "main.h",
        "main.cc",
        "main.cpp",
        "main.cxx",
        "main.hh",
        "main.hpp",
        "main.hxx",
        "main.cs",
        "Main.java",
        "Main.kt",
        "build.kts",
        "main.swift",
        "schema.sql",
        "api.proto",
    ];
    for path in source_paths {
        fs::write(repo.path().join(path), "needle\n").expect("write source");
    }

    let output = search_source(search_options(repo.path(), "needle")).expect("search");

    assert_eq!(output.coverage.total_matches, source_paths.len());
}

#[test]
fn walk_depth_limit_marks_coverage_incomplete() {
    let repo = tempfile::tempdir().expect("tempdir");
    let mut directory = repo.path().to_path_buf();
    for depth in 0..=1 {
        directory = directory.join(format!("d{depth}"));
    }
    fs::create_dir_all(&directory).expect("deep source directory");
    fs::write(directory.join("deep.rs"), "needle\n").expect("deep source");

    let output = search_source_with_walk_limits(
        search_options(repo.path(), "needle"),
        SourceWalkLimits {
            max_depth: 1,
            max_directories: SOURCE_SEARCH_MAX_WALK_DIRECTORIES,
            max_entries: SOURCE_SEARCH_MAX_WALK_ENTRIES,
        },
    )
    .expect("search");

    assert_eq!(output.coverage.total_matches, 0);
    assert_eq!(
        output.truncated_reason,
        Some(SourceTruncatedReason::WalkLimit)
    );
}

#[test]
fn walk_directory_limit_stops_before_descending() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::create_dir(repo.path().join("nested")).expect("nested directory");
    fs::write(repo.path().join("nested/deep.rs"), "needle\n").expect("deep source");

    let output = search_source_with_walk_limits(
        search_options(repo.path(), "needle"),
        SourceWalkLimits {
            max_depth: SOURCE_SEARCH_MAX_WALK_DEPTH,
            max_directories: 1,
            max_entries: SOURCE_SEARCH_MAX_WALK_ENTRIES,
        },
    )
    .expect("search");

    assert_eq!(output.coverage.total_matches, 0);
    assert_eq!(
        output.truncated_reason,
        Some(SourceTruncatedReason::WalkLimit)
    );
}

#[test]
fn walk_entry_limit_stops_before_examining_an_extra_entry() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("a.rs"), "needle a\n").expect("first source");
    fs::write(repo.path().join("b.rs"), "needle b\n").expect("second source");

    let output = search_source_with_walk_limits(
        search_options(repo.path(), "needle"),
        SourceWalkLimits {
            max_depth: SOURCE_SEARCH_MAX_WALK_DEPTH,
            max_directories: SOURCE_SEARCH_MAX_WALK_DIRECTORIES,
            max_entries: 1,
        },
    )
    .expect("search");

    assert_eq!(output.coverage.files_scanned, 1);
    assert_eq!(output.coverage.total_matches, 1);
    assert_eq!(
        output.truncated_reason,
        Some(SourceTruncatedReason::WalkLimit)
    );
}

#[test]
fn walk_entry_limit_is_shared_across_explicit_roots() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(repo.path().join("a")).expect("first root");
    fs::create_dir_all(repo.path().join("b")).expect("second root");
    fs::write(repo.path().join("a/first.rs"), "needle first\n").expect("first source");
    fs::write(repo.path().join("b/second.rs"), "needle second\n").expect("second source");
    let mut options = search_options(repo.path(), "needle");
    options.roots = vec![PathBuf::from("a"), PathBuf::from("b")];

    let output = search_source_with_walk_limits(
        options,
        SourceWalkLimits {
            max_depth: SOURCE_SEARCH_MAX_WALK_DEPTH,
            max_directories: SOURCE_SEARCH_MAX_WALK_DIRECTORIES,
            max_entries: 1,
        },
    )
    .expect("search");

    assert_eq!(output.coverage.files_scanned, 1);
    assert_eq!(output.coverage.total_matches, 1);
    assert_eq!(
        output.truncated_reason,
        Some(SourceTruncatedReason::WalkLimit)
    );
}

#[test]
fn files_over_per_file_budget_are_skipped_without_consuming_scan_bytes() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(
        repo.path().join("large.rs"),
        vec![b'x'; SOURCE_SEARCH_MAX_FILE_BYTES + 1],
    )
    .expect("write");

    let output = search_source(search_options(repo.path(), "needle")).expect("search");

    assert_eq!(output.coverage.files_scanned, 1);
    assert_eq!(output.coverage.files_skipped_too_large, 1);
    assert_eq!(output.coverage.files_skipped_non_utf8, 0);
    assert_eq!(output.coverage.bytes_scanned, 0);
    assert!(output.truncated);
    assert_eq!(
        output.truncated_reason,
        Some(SourceTruncatedReason::OversizedFiles)
    );
}

#[test]
fn non_utf8_files_are_reported_as_incomplete_coverage() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("encoded.rs"), [0xff, 0xfe, b'n', b'e'])
        .expect("write non-UTF-8 source");

    let output = search_source(search_options(repo.path(), "needle")).expect("search");

    assert_eq!(output.coverage.files_scanned, 1);
    assert_eq!(output.coverage.files_skipped_too_large, 0);
    assert_eq!(output.coverage.files_skipped_non_utf8, 1);
    assert!(output.truncated);
    assert_eq!(
        output.truncated_reason,
        Some(SourceTruncatedReason::NonUtf8Files)
    );
}

#[test]
fn read_span_is_one_based_bounded_and_reports_route() {
    let repo = tempfile::tempdir().expect("tempdir");
    let path = repo.path().join("codex-rs/file-search/src/lib.rs");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, "one\ntwo\nthree\nfour\n").expect("write");

    let output = read_file_span(ReadFileSpanOptions {
        repo_root: repo.path().to_path_buf(),
        path: PathBuf::from("codex-rs/file-search/src/lib.rs"),
        start_line: 2,
        line_count: 2,
    })
    .expect("read");

    assert_eq!(output.start_line, Some(2));
    assert_eq!(output.end_line, Some(3));
    assert_eq!(output.total_lines, 4);
    assert_eq!(
        output.lines,
        vec![
            SourceLine {
                line_number: 2,
                text: "two".to_string(),
                text_truncated: false,
            },
            SourceLine {
                line_number: 3,
                text: "three".to_string(),
                text_truncated: false,
            },
        ]
    );
    assert_eq!(output.source_map_route, Some("file-search".to_string()));
    assert!(!output.truncated);
}

#[test]
fn read_span_rejects_invalid_line_count_and_outside_files() {
    let parent = tempfile::tempdir().expect("tempdir");
    let repo = parent.path().join("repo");
    fs::create_dir_all(&repo).expect("repo");
    let source = (1..=SOURCE_READ_MAX_LINES + 5)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(repo.join("source.rs"), source).expect("source");
    let outside = parent.path().join("outside.rs");
    fs::write(&outside, "outside\n").expect("outside");

    let limit_error = read_file_span(ReadFileSpanOptions {
        repo_root: repo.clone(),
        path: PathBuf::from("source.rs"),
        start_line: 1,
        line_count: SOURCE_READ_MAX_LINES + 100,
    })
    .expect_err("excessive line count rejected");
    assert!(
        limit_error
            .to_string()
            .contains("line_count must be between")
    );

    let outside_error = read_file_span(ReadFileSpanOptions {
        repo_root: repo,
        path: outside,
        start_line: 1,
        line_count: 1,
    })
    .expect_err("outside path rejected");
    assert!(
        outside_error
            .to_string()
            .contains("outside repository root")
    );
}

#[test]
fn representative_large_repository_search_stays_within_walk_and_output_bounds() {
    let repo = tempfile::tempdir().expect("tempdir");
    let source = repo.path().join("src");
    fs::create_dir(&source).expect("source directory");
    for index in 0..512 {
        fs::write(
            source.join(format!("module_{index:04}.rs")),
            format!("pub fn readiness_needle_{index:04}() {{}}\n"),
        )
        .expect("write representative source file");
    }
    let mut options = search_options(repo.path(), "readiness_needle");
    options.roots = vec![PathBuf::from("src")];

    let output = search_source_with_walk_limits(
        options,
        SourceWalkLimits {
            max_depth: SOURCE_SEARCH_MAX_WALK_DEPTH,
            max_directories: SOURCE_SEARCH_MAX_WALK_DIRECTORIES,
            max_entries: 64,
        },
    )
    .expect("bounded representative search");

    assert_eq!(
        output.truncated_reason,
        Some(SourceTruncatedReason::WalkLimit)
    );
    assert!(output.coverage.files_scanned <= 64);
    assert!(output.coverage.matches_returned <= SOURCE_SEARCH_DEFAULT_MAX_MATCHES);
    assert!(output.coverage.result_bytes <= SOURCE_SEARCH_MAX_RESULT_BYTES);
}

#[test]
fn capped_unscoped_miss_reports_only_the_scanned_portion() {
    let repo = tempfile::tempdir().expect("tempdir");
    for index in 0..4 {
        fs::write(
            repo.path().join(format!("source_{index}.rs")),
            "pub fn unrelated() {}\n",
        )
        .expect("write source");
    }

    let output = search_source_with_walk_limits(
        search_options(repo.path(), "absent_needle"),
        SourceWalkLimits {
            max_depth: SOURCE_SEARCH_MAX_WALK_DEPTH,
            max_directories: SOURCE_SEARCH_MAX_WALK_DIRECTORIES,
            max_entries: 1,
        },
    )
    .expect("bounded unscoped miss");

    assert!(!output.coverage_complete);
    assert!(!output.coverage.index_complete);
    assert!(output.matches.is_empty());
    let note = output.coverage_note.expect("incomplete coverage note");
    assert!(note.contains("No matches were found in the scanned portion"));
    assert!(note.contains("Narrow `paths` or use `locate_task`"));
}

#[test]
fn capped_unscoped_hit_still_reports_incomplete_coverage() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("a_match.rs"), "pub fn needle() {}\n").expect("write match");
    fs::write(repo.path().join("z_unvisited.rs"), "pub fn later() {}\n")
        .expect("write unvisited source");

    let output = search_source_with_walk_limits(
        search_options(repo.path(), "needle"),
        SourceWalkLimits {
            max_depth: SOURCE_SEARCH_MAX_WALK_DEPTH,
            max_directories: SOURCE_SEARCH_MAX_WALK_DIRECTORIES,
            max_entries: 1,
        },
    )
    .expect("bounded unscoped hit");

    assert_eq!(output.coverage.total_matches, 1);
    assert!(!output.coverage_complete);
    assert!(
        output
            .coverage_note
            .as_deref()
            .is_some_and(|note| note.contains("matches cover only the scanned portion"))
    );
}

#[test]
fn scoped_search_stays_within_owner_bounds_and_reports_counter_invariants() {
    let repo = tempfile::tempdir().expect("tempdir");
    let owner = repo.path().join("owner");
    let elsewhere = repo.path().join("elsewhere");
    fs::create_dir(&owner).expect("owner directory");
    fs::create_dir(&elsewhere).expect("elsewhere directory");
    for index in 0..5 {
        fs::write(
            owner.join(format!("owned_{index}.rs")),
            format!("pub fn owned_{index}() {{}}\n"),
        )
        .expect("write owned source");
    }
    fs::write(owner.join("expected.rs"), "pub fn expected_needle() {}\n")
        .expect("write expected source");
    for index in 0..30 {
        fs::write(
            elsewhere.join(format!("unowned_{index}.rs")),
            "pub fn expected_needle() {}\n",
        )
        .expect("write unowned source");
    }
    let mut options = search_options(repo.path(), "expected_needle");
    options.roots = vec![PathBuf::from("owner")];

    let output = search_source(options).expect("scoped search");

    assert_eq!(output.coverage.total_matches, 1);
    assert!(
        output
            .matches
            .iter()
            .all(|found| found.path.starts_with("owner/"))
    );
    assert!(output.coverage.files_scanned <= 20);
    assert!(output.coverage.bytes_scanned <= 500 * 1024);
    assert!(!output.truncated);
    assert!(output.coverage_complete);
    assert!(output.coverage.walked_entries >= output.coverage.files_scanned);
    assert!(output.coverage.ignored_entries <= output.coverage.walked_entries);
    assert!(output.diagnostics.traversal_micros <= output.diagnostics.total_micros);
    assert!(output.diagnostics.file_scan_match_micros <= output.diagnostics.traversal_micros);
    assert!(output.diagnostics.projection_micros <= output.diagnostics.total_micros);
    assert!(
        output
            .diagnostics
            .first_match_micros
            .is_some_and(|duration| duration <= output.diagnostics.total_micros)
    );
}

#[test]
fn projection_is_deterministic_and_keeps_all_sorted_match_identities() {
    let repo = tempfile::tempdir().expect("tempdir");
    let line = format!("needle {}\n", "x".repeat(SOURCE_SEARCH_MAX_LINE_BYTES));
    fs::write(repo.path().join("b.rs"), line.repeat(100)).expect("write b");
    fs::write(repo.path().join("a.rs"), line.repeat(100)).expect("write a");
    let mut options = search_options(repo.path(), "needle");
    options.max_matches = SOURCE_SEARCH_MAX_MATCHES;

    let first = search_source(options.clone()).expect("first search");
    let second = search_source(options).expect("second search");

    assert_eq!(first, second);
    assert_eq!(
        first.truncated_reason,
        Some(SourceTruncatedReason::MaxResultBytes)
    );
    assert_eq!(first.matches.len(), 200);
    assert!(!first.coverage.result_cap_reached);
    assert!(first.coverage.index_complete);
    assert!(!first.coverage.context_complete);
    assert!(first.coverage.omitted_contexts > 0);
    assert!(first.matches.windows(2).all(|pair| {
        pair[0]
            .path
            .cmp(&pair[1].path)
            .then_with(|| pair[0].line_number.cmp(&pair[1].line_number))
            .is_le()
    }));
    assert_eq!(
        first.coverage.result_bytes,
        serde_json::to_vec_pretty(&first)
            .expect("serialize projected output")
            .len()
            + 1
    );
}
