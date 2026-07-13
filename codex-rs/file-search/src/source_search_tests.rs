use super::*;
use pretty_assertions::assert_eq;
use std::fs;

fn search_options(repo_root: &Path, query: &str) -> SourceSearchOptions {
    SourceSearchOptions::new(repo_root.to_path_buf(), query.to_string())
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
fn fixed_string_search_treats_punctuation_literally() {
    let repo = tempfile::tempdir().expect("tempdir");
    fs::write(repo.path().join("source.rs"), "alpha.beta\nalphaXbeta\n").expect("write");

    let output = search_source(search_options(repo.path(), "alpha.beta")).expect("search");

    assert_eq!(output.coverage.total_matches, 1);
    assert_eq!(output.matches[0].line_number, 1);
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
            .all(|source_match| source_match.lines[0].text.len() <= SOURCE_SEARCH_MAX_LINE_BYTES)
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
fn read_span_caps_lines_and_rejects_outside_files() {
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

    let output = read_file_span(ReadFileSpanOptions {
        repo_root: repo.clone(),
        path: PathBuf::from("source.rs"),
        start_line: 1,
        line_count: SOURCE_READ_MAX_LINES + 100,
    })
    .expect("read");
    assert_eq!(output.lines.len(), SOURCE_READ_MAX_LINES);
    assert!(output.truncated);

    let error = read_file_span(ReadFileSpanOptions {
        repo_root: repo,
        path: outside,
        start_line: 1,
        line_count: 1,
    })
    .expect_err("outside path rejected");
    assert!(error.to_string().contains("outside repository root"));
}
