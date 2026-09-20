use super::approx_token_count;
use super::approx_token_count_exceeds;
use super::split_string;
use super::truncate_middle_chars;
use super::truncate_middle_with_token_budget;
use pretty_assertions::assert_eq;

#[test]
fn token_estimates_preserve_unicode_and_ascii_boundaries() {
    for (text, expected) in [
        ("", 0),
        ("abcd", 1),
        ("abcde", 2),
        ("a b c", 3),
        ("!!!", 3),
        ("_abcd", 2),
        ("a\u{b}b", 2),
        ("é雪_1", 2),
        ("a\u{a0}b", 2),
        ("a💥b", 3),
        ("e\u{301}", 2),
        ("  \t\r\n  ", 2),
    ] {
        assert_eq!(approx_token_count(text), expected, "{text:?}");
        for limit in 0..=expected + 1 {
            assert_eq!(
                approx_token_count_exceeds(text, limit),
                expected > limit,
                "{text:?}, {limit}"
            );
        }
        assert!(!approx_token_count_exceeds(text, usize::MAX));
    }
}

#[test]
fn token_estimates_match_character_reference_for_all_scalar_values() {
    // Include ASCII/Unicode seams, word endings, whitespace and punctuation.
    for ch in (0..=0x10ffff).filter_map(char::from_u32) {
        let text = format!("abcd{ch}ef!");
        let mut lexical = 0usize;
        let mut word_bytes = 0usize;
        for ch in text.chars() {
            if ch.is_alphanumeric() || ch == '_' {
                word_bytes += ch.len_utf8();
            } else {
                lexical += word_bytes.div_ceil(4) + usize::from(!ch.is_whitespace());
                word_bytes = 0;
            }
        }
        let expected = text.len().div_ceil(4).max(lexical + word_bytes.div_ceil(4));
        assert_eq!(approx_token_count(&text), expected, "{ch:?}");
        assert!(approx_token_count_exceeds(&text, expected - 1), "{ch:?}");
        assert!(!approx_token_count_exceeds(&text, expected), "{ch:?}");
    }
}

#[test]
fn token_budget_includes_marker_for_small_positive_budgets() {
    for input in ["abcdef".to_string(), "雪!".repeat(100)] {
        for budget in 1..=20 {
            let original_count = approx_token_count(&input);
            let (output, original) = truncate_middle_with_token_budget(&input, budget);
            assert!(
                approx_token_count(&output) <= budget,
                "budget={budget}: {output}"
            );
            assert_eq!(
                original,
                (original_count > budget).then_some(original_count as u64)
            );
            if original_count <= budget {
                assert_eq!(output, input);
            }
        }
    }
}

#[test]
fn token_budget_handles_dense_ends_and_large_sparse_middle() {
    let input = format!(
        "{}{}{}",
        "!".repeat(5_000),
        "a".repeat(990_000),
        "!".repeat(5_000)
    );
    let (output, original) = truncate_middle_with_token_budget(&input, 2_000);
    assert_eq!(original, Some(approx_token_count(&input) as u64));
    assert!(approx_token_count(&output) <= 2_000);
    assert!(output.starts_with('!') && output.ends_with('!'));
    assert!(output.contains("tokens truncated"));
}

#[test]
fn split_string_works() {
    assert_eq!(
        split_string(
            "hello world",
            /*beginning_bytes*/ 5,
            /*end_bytes*/ 5
        ),
        (1, "hello", "world")
    );
    assert_eq!(
        split_string("abc", /*beginning_bytes*/ 0, /*end_bytes*/ 0),
        (3, "", "")
    );
}

#[test]
fn split_string_handles_empty_string() {
    assert_eq!(
        split_string("", /*beginning_bytes*/ 4, /*end_bytes*/ 4),
        (0, "", "")
    );
}

#[test]
fn split_string_only_keeps_prefix_when_tail_budget_is_zero() {
    assert_eq!(
        split_string("abcdef", /*beginning_bytes*/ 3, /*end_bytes*/ 0),
        (3, "abc", "")
    );
}

#[test]
fn split_string_only_keeps_suffix_when_prefix_budget_is_zero() {
    assert_eq!(
        split_string("abcdef", /*beginning_bytes*/ 0, /*end_bytes*/ 3),
        (3, "", "def")
    );
}

#[test]
fn split_string_handles_overlapping_budgets_without_removal() {
    assert_eq!(
        split_string("abcdef", /*beginning_bytes*/ 4, /*end_bytes*/ 4),
        (0, "abcd", "ef")
    );
}

#[test]
fn split_string_respects_utf8_boundaries() {
    assert_eq!(
        split_string("😀abc😀", /*beginning_bytes*/ 5, /*end_bytes*/ 5),
        (1, "😀a", "c😀")
    );

    assert_eq!(
        split_string(
            "😀😀😀😀😀",
            /*beginning_bytes*/ 1,
            /*end_bytes*/ 1
        ),
        (5, "", "")
    );
    assert_eq!(
        split_string(
            "😀😀😀😀😀",
            /*beginning_bytes*/ 7,
            /*end_bytes*/ 7
        ),
        (3, "😀", "😀")
    );
    assert_eq!(
        split_string(
            "😀😀😀😀😀",
            /*beginning_bytes*/ 8,
            /*end_bytes*/ 8
        ),
        (1, "😀😀", "😀😀")
    );
}

#[test]
fn truncate_with_token_budget_returns_original_when_under_limit() {
    let s = "short output";
    let limit = 100;
    let (out, original) = truncate_middle_with_token_budget(s, limit);
    assert_eq!(out, s);
    assert_eq!(original, None);
}

#[test]
fn truncate_with_token_budget_reports_truncation_at_zero_limit() {
    let s = "abcdef";
    let (out, original) = truncate_middle_with_token_budget(s, /*max_tokens*/ 0);
    assert_eq!(out, "");
    assert_eq!(original, Some(2));
}

#[test]
fn truncate_middle_tokens_handles_utf8_content() {
    let s = "😀😀😀😀😀😀😀😀😀😀\nsecond line with text\n";
    let (out, tokens) = truncate_middle_with_token_budget(s, /*max_tokens*/ 12);
    assert!(out.starts_with("😀"));
    assert!(out.ends_with("text\n"));
    assert!(approx_token_count(&out) <= 12);
    assert_eq!(tokens, Some(16));
}

#[test]
fn token_estimate_is_conservative_for_punctuation_heavy_code() {
    let json = r#"{"a":[1,2,3],"b":{"c":true}}"#;

    assert!(approx_token_count(json) > json.len().div_ceil(4));
    let (out, original) = truncate_middle_with_token_budget(json, 10);
    assert!(original.is_some());
    assert!(approx_token_count(&out) <= 10);
}

#[test]
fn truncate_middle_bytes_handles_utf8_content() {
    let s = "😀😀😀😀😀😀😀😀😀😀\nsecond line with text\n";
    let out = truncate_middle_chars(s, /*max_bytes*/ 20);
    assert_eq!(out, "😀😀…21 chars truncated…with text\n");
}
