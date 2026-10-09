//! Collects markdown stream source at newline boundaries.
//!
//! `MarkdownStreamCollector` buffers incoming token deltas and exposes a commit boundary at each
//! newline. The stream controllers (`streaming/controller.rs`) call `commit_complete_source()`
//! after each newline-bearing delta to obtain the completed prefix for re-rendering, leaving the
//! trailing incomplete line in the buffer for the next delta.
//!
//! On finalization, `finalize_and_drain_source()` flushes whatever remains (the last line, which
//! may lack a trailing newline).

#[cfg(test)]
use ratatui::text::Line;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

/// Newline-gated accumulator that buffers raw markdown source and commits only completed lines.
///
/// Completed source is transferred to the stream controller; only the incomplete suffix remains.
///
/// The collector does not parse markdown in production. It only defines stable source boundaries;
/// rendering lives in the stream controllers so width changes can re-render from one accumulated
/// source string.
pub(crate) struct MarkdownStreamCollector {
    buffer: String,
    width: Option<usize>,
}

impl MarkdownStreamCollector {
    /// Create a collector that accumulates raw markdown deltas.
    ///
    /// Rendering belongs to the stream controller; commits operate only on raw source boundaries.
    pub fn new(width: Option<usize>, cwd: &Path) -> Self {
        let _ = cwd;

        Self {
            buffer: String::new(),
            width,
        }
    }

    /// Track the rendering width supplied by the stream controller.
    pub fn set_width(&mut self, width: Option<usize>) {
        self.width = width;
    }

    /// Reset all buffered source and commit bookkeeping.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    /// Append a raw streaming delta to the internal source buffer.
    pub fn push_delta(&mut self, delta: &str) {
        tracing::trace!("push_delta: {delta:?}");
        self.buffer.push_str(delta);
    }

    /// Commit newly completed raw markdown source up to the last newline.
    ///
    /// This returns only source that has not been returned by a previous commit. Calling it after a
    /// delta without a newline returns `None`, which prevents the live stream from rendering
    /// incomplete markdown blocks that may change meaning when the rest of the line arrives.
    pub fn commit_complete_source(&mut self) -> Option<String> {
        let commit_end = self.buffer.rfind('\n').map(|idx| idx + 1)?;
        let remainder = self.buffer.split_off(commit_end);
        let out = std::mem::replace(&mut self.buffer, remainder);
        Some(out)
    }

    /// Finalize the stream and return any remaining raw source.
    ///
    /// Ensures the returned source chunk is newline-terminated when non-empty so callers can
    /// safely run markdown block parsing on the final chunk. This method clears the collector;
    /// callers should not invoke it until the stream is truly complete or interrupted output is
    /// being intentionally consolidated.
    pub fn finalize_and_drain_source(&mut self) -> String {
        if self.buffer.is_empty() {
            self.clear();
            return String::new();
        }

        let mut out = std::mem::take(&mut self.buffer);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        self.clear();
        out
    }

}

#[cfg(test)]
fn test_cwd() -> PathBuf {
    // These tests only need a stable absolute cwd; using temp_dir() avoids baking Unix- or
    // Windows-specific root semantics into the fixtures.
    std::env::temp_dir()
}

#[cfg(test)]
pub(crate) fn simulate_stream_markdown_for_tests(
    deltas: &[&str],
    finalize: bool,
) -> Vec<Line<'static>> {
    let mut controller = crate::streaming::controller::StreamController::new(
        None,
        &test_cwd(),
        codex_config::types::UriBasedFileOpener::None,
        crate::history_cell::HistoryRenderMode::Rich,
    );
    let mut out = Vec::new();
    for delta in deltas {
        controller.push(delta);
        controller.flush_render_for_frame();
        if let (Some(cell), _) = controller.on_commit_tick_batch(usize::MAX) {
            out.extend(cell.transcript_lines(u16::MAX));
        }
    }
    if finalize
        && let (Some(cell), _) = controller.finalize()
    {
        out.extend(cell.transcript_lines(u16::MAX));
    }
    // Remove the UI's two-column message prefix, retaining the renderer's span styles.
    for line in &mut out {
        let mut remaining = 2;
        for span in &mut line.spans {
            let count = span.content.chars().count().min(remaining);
            span.content = span.content.chars().skip(count).collect::<String>().into();
            remaining -= count;
            if remaining == 0 {
                break;
            }
        }
        assert_eq!(remaining, 0, "missing stream message prefix: {line:?}");
        line.spans.retain(|span| !span.content.is_empty());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn no_commit_until_newline() {
        let mut collector = MarkdownStreamCollector::new(None, &test_cwd());
        collector.push_delta("Hello, world");
        assert_eq!(collector.commit_complete_source(), None);
        collector.push_delta("!\npartial");
        assert_eq!(
            collector.commit_complete_source(),
            Some("Hello, world!\n".to_string())
        );
        assert_eq!(collector.commit_complete_source(), None);
        collector.push_delta(" line\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("partial line\n".to_string())
        );
        assert_eq!(collector.finalize_and_drain_source(), "");
    }

    #[test]
    fn finalize_commits_partial_line() {
        let mut collector = MarkdownStreamCollector::new(None, &test_cwd());
        collector.push_delta("committed\nremaining é");
        assert_eq!(
            collector.commit_complete_source(),
            Some("committed\n".to_string())
        );
        assert_eq!(collector.finalize_and_drain_source(), "remaining é\n");
        assert_eq!(collector.finalize_and_drain_source(), "");
        assert_eq!(collector.commit_complete_source(), None);
        collector.push_delta("next stream\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("next stream\n".to_string())
        );
        collector.clear();
        assert_eq!(collector.finalize_and_drain_source(), "");
    }

    #[test]
    fn e2e_stream_blockquotes_preserve_content_and_green_style() {
        for (source, expected) in [
            ("> Hello\n", vec!["> Hello"]),
            ("> Level 1\n>> Level 2\n", vec!["> Level 1", "> > Level 2"]),
            ("> - item 1\n> - item 2\n", vec!["> - item 1", "> - item 2"]),
        ] {
            let out = simulate_stream_markdown_for_tests(&[source], true);
            let non_blank = out.into_iter().filter(|line| {
                let text = line.to_string();
                !text.trim().is_empty() && text.trim() != ">"
            }).collect::<Vec<_>>();
            assert_eq!(lines_to_plain_strings(&non_blank), expected, "{source:?}");
            for line in non_blank {
                assert_eq!(line.style.fg, Some(Color::Green), "{line:?}");
            }
        }
    }

    #[test]
    fn e2e_stream_blockquote_wrap_preserves_green_style() {
        let long = "> This is a very long quoted line that should wrap across multiple columns to verify style preservation.";
        let out = super::simulate_stream_markdown_for_tests(&[long, "\n"], /*finalize*/ true);
        // Wrap to a narrow width to force multiple output lines.
        let wrapped = crate::wrapping::word_wrap_lines(
            out.iter(),
            crate::wrapping::RtOptions::new(/*width*/ 24),
        );
        // Filter out purely blank lines
        let non_blank: Vec<_> = wrapped
            .into_iter()
            .filter(|l| {
                let s = l
                    .spans
                    .iter()
                    .map(|sp| sp.content.clone())
                    .collect::<Vec<_>>()
                    .join("");
                !s.trim().is_empty()
            })
            .collect();
        assert!(
            non_blank.len() >= 2,
            "expected wrapped blockquote to span multiple lines"
        );
        for (i, l) in non_blank.iter().enumerate() {
            assert_eq!(
                l.style.patch(l.spans[0].style).fg,
                Some(Color::Green),
                "wrapped line {} should preserve green style, got {:?}",
                i,
                l.spans[0].style.fg
            );
        }
    }

    #[test]
    fn headings_wait_for_newline_and_remain_separate_from_paragraphs() {
        for (deltas, expected) in [
            (vec!["Hello.\n", "## Heading\n"], vec!["Hello.", "", "## Heading"]),
            (
                vec!["Sounds good!", "\n## Adding Bird subcommand", "\n"],
                vec!["Sounds good!", "", "## Adding Bird subcommand"],
            ),
        ] {
            assert_eq!(
                lines_to_plain_strings(&simulate_stream_markdown_for_tests(&deltas, false)),
                expected,
            );
        }
        assert!(simulate_stream_markdown_for_tests(&["Sounds good!"], false).is_empty());
        assert_eq!(
            lines_to_plain_strings(&simulate_stream_markdown_for_tests(
                &["Sounds good!", "\n## Adding Bird subcommand"],
                false,
            )),
            ["Sounds good!"],
        );
    }

    fn lines_to_plain_strings(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    #[test]
    fn lists_and_fences_commit_without_duplication() {
        // List case
        assert_streamed_equals_full(&["- a\n- ", "b\n- c\n"]);

        // Fenced code case: stream in small chunks
        assert_streamed_equals_full(&["```", "\nco", "de 1\ncode 2\n", "```\n"]);
    }

    #[test]
    fn utf8_boundary_safety_and_wide_chars() {
        // Emoji (wide), CJK, control char, digit + combining macron sequences
        let input = "🙂🙂🙂\n汉字漢字\nA\u{0003}0\u{0304}\n";
        let deltas = vec![
            "🙂",
            "🙂",
            "🙂\n汉",
            "字漢",
            "字\nA",
            "\u{0003}",
            "0",
            "\u{0304}",
            "\n",
        ];

        let streamed = simulate_stream_markdown_for_tests(&deltas, /*finalize*/ true);
        let streamed_str = lines_to_plain_strings(&streamed);

        let mut rendered_all: Vec<ratatui::text::Line<'static>> = Vec::new();
        let test_cwd = super::test_cwd();
        crate::markdown::append_markdown(
            input,
            /*width*/ None,
            Some(test_cwd.as_path()),
            &mut rendered_all,
        );
        let rendered_all_str = lines_to_plain_strings(&rendered_all);

        assert_eq!(
            streamed_str, rendered_all_str,
            "utf8/wide-char streaming should equal full render without duplication or truncation"
        );
    }

    #[test]
    fn e2e_stream_deep_nested_third_level_marker_is_light_blue() {
        let md = "1. First\n   - Second level\n     1. Third level (ordered)\n        - Fourth level (bullet)\n          - Fifth level to test indent consistency\n";
        for deltas in [vec![md], md.split_inclusive('\n').collect::<Vec<_>>()] {
        let streamed = super::simulate_stream_markdown_for_tests(&deltas, /*finalize*/ true);
        let streamed_strs = lines_to_plain_strings(&streamed);

        // Locate the third-level line in the streamed output; avoid relying on exact indent.
        let target_suffix = "1. Third level (ordered)";
        let mut found = None;
        for line in &streamed {
            let s: String = line.spans.iter().map(|sp| sp.content.clone()).collect();
            if s.contains(target_suffix) {
                found = Some(line.clone());
                break;
            }
        }
        let line = found.unwrap_or_else(|| {
            panic!("expected to find the third-level ordered list line; got: {streamed_strs:?}")
        });

        // The marker (including indent and "1.") is expected to be in the first span
        // and colored LightBlue; following content should be default color.
        assert!(
            !line.spans.is_empty(),
            "expected non-empty spans for the third-level line"
        );
        let marker_span = &line.spans[0];
        assert_eq!(
            marker_span.style.fg,
            Some(Color::LightBlue),
            "expected LightBlue 3rd-level ordered marker, got {:?}",
            marker_span.style.fg
        );
        // Find the first non-empty non-space content span and verify it is default color.
        let mut content_fg = None;
        for sp in &line.spans[1..] {
            let t = sp.content.trim();
            if !t.is_empty() {
                content_fg = Some(sp.style.fg);
                break;
            }
        }
        assert_eq!(
            content_fg,
            Some(None),
            "expected default color for 3rd-level content, got {content_fg:?}"
        );
        }
    }

    #[test]
    fn empty_fenced_block_is_dropped_and_separator_preserved_before_heading() {
        // An empty fenced code block followed by a heading should not render the fence,
        // but should preserve a blank separator line so the heading starts on a new line.
        let deltas = vec!["```bash\n```\n", "## Heading\n"]; // empty block and close in same commit
        let streamed = simulate_stream_markdown_for_tests(&deltas, /*finalize*/ true);
        let texts = lines_to_plain_strings(&streamed);
        assert!(
            texts.iter().all(|s| !s.contains("```")),
            "no fence markers expected: {texts:?}"
        );
        // Expect the heading and no fence markers. A blank separator may or may not be rendered at start.
        assert!(
            texts.iter().any(|s| s == "## Heading"),
            "expected heading line: {texts:?}"
        );
    }

    #[test]
    fn paragraph_then_empty_fence_then_heading_keeps_heading_on_new_line() {
        let deltas = vec!["Para.\n", "```\n```\n", "## Title\n"]; // empty fence block in one commit
        let streamed = simulate_stream_markdown_for_tests(&deltas, /*finalize*/ true);
        let texts = lines_to_plain_strings(&streamed);
        let para_idx = match texts.iter().position(|s| s == "Para.") {
            Some(i) => i,
            None => panic!("para present"),
        };
        let head_idx = match texts.iter().position(|s| s == "## Title") {
            Some(i) => i,
            None => panic!("heading present"),
        };
        assert!(
            head_idx > para_idx,
            "heading should not merge with paragraph: {texts:?}"
        );
    }

    #[test]
    fn loose_list_with_split_dashes_matches_full_render() {
        // Minimized failing sequence discovered by the helper: two chunks
        // that still reproduce the mismatch.
        let deltas = vec!["- item.\n\n", "-"];

        let streamed = simulate_stream_markdown_for_tests(&deltas, /*finalize*/ true);
        let streamed_strs = lines_to_plain_strings(&streamed);

        let full: String = deltas.iter().copied().collect();
        let mut rendered_all: Vec<ratatui::text::Line<'static>> = Vec::new();
        let test_cwd = super::test_cwd();
        crate::markdown::append_markdown(
            &full,
            /*width*/ None,
            Some(test_cwd.as_path()),
            &mut rendered_all,
        );
        let rendered_all_strs = lines_to_plain_strings(&rendered_all);

        assert_eq!(
            streamed_strs, rendered_all_strs,
            "streamed output should match full render without dangling '-' lines"
        );
    }

    // Targeted tests derived from fuzz findings. Each asserts streamed == full render.
    fn assert_streamed_equals_full(deltas: &[&str]) {
        let streamed = simulate_stream_markdown_for_tests(deltas, /*finalize*/ true);
        let streamed_strs = lines_to_plain_strings(&streamed);
        let full: String = deltas.iter().copied().collect();
        let mut rendered: Vec<ratatui::text::Line<'static>> = Vec::new();
        let test_cwd = super::test_cwd();
        crate::markdown::append_markdown(
            &full,
            /*width*/ None,
            Some(test_cwd.as_path()),
            &mut rendered,
        );
        let rendered_strs = lines_to_plain_strings(&rendered);
        assert_eq!(streamed_strs, rendered_strs, "full:\n---\n{full}\n---");
    }

    #[test]
    fn fuzz_class_bullet_duplication_variant_1() {
        assert_streamed_equals_full(&[
            "aph.\n- let one\n- bull",
            "et two\n\n  second paragraph \n",
        ])
        ;
    }

    #[test]
    fn fuzz_class_bullet_duplication_variant_2() {
        assert_streamed_equals_full(&[
            "- e\n  c",
            "e\n- bullet two\n\n  second paragraph in bullet two\n",
        ])
        ;
    }

    #[test]
    fn streaming_html_block_then_text_matches_full() {
        assert_streamed_equals_full(&[
            "HTML block:\n",
            "<div>inline block</div>\n",
            "more stuff\n",
        ])
        ;
    }

    #[test]
    fn table_like_lines_inside_fenced_code_are_not_held() {
        assert_streamed_equals_full(&["```\n", "| a | b |\n", "```\n"]);
    }

    #[test]
    fn collector_source_chunks_round_trip_into_agent_fence_unwrapping() {
        let deltas = [
            "```md\n",
            "| A | B |\n",
            "|---|---|\n",
            "| 1 | 2 |\n",
            "```\n",
        ];
        let mut collector =
            super::MarkdownStreamCollector::new(/*width*/ None, &super::test_cwd());
        let mut raw_source = String::new();

        for delta in deltas {
            collector.push_delta(delta);
            if delta.contains('\n')
                && let Some(chunk) = collector.commit_complete_source()
            {
                raw_source.push_str(&chunk);
            }
        }
        raw_source.push_str(&collector.finalize_and_drain_source());

        let mut rendered = Vec::new();
        crate::markdown::append_markdown_agent(&raw_source, /*width*/ None, &mut rendered);
        let rendered_strs = lines_to_plain_strings(&rendered);

        assert!(
            rendered_strs.iter().any(|line| line.contains('━')),
            "expected markdown-fenced table to render with a separator: {rendered_strs:?}"
        );
        assert!(
            !rendered_strs.iter().any(|line| line.trim() == "| A | B |"),
            "did not expect raw table header after markdown-fence unwrapping: {rendered_strs:?}"
        );
    }
    #[test]
    fn commits_transfer_source_and_retain_only_incomplete_suffix() {
        let mut collector = MarkdownStreamCollector::new(None, &test_cwd());
        collector.push_delta("first\nsec");
        assert_eq!(
            collector.commit_complete_source(),
            Some("first\n".to_string())
        );
        assert_eq!(collector.buffer, "sec");
        assert_eq!(collector.commit_complete_source(), None);
        collector.push_delta("ond\nlast");
        assert_eq!(
            collector.commit_complete_source(),
            Some("second\n".to_string())
        );
        assert_eq!(collector.buffer, "last");
        assert_eq!(collector.finalize_and_drain_source(), "last\n");
        assert_eq!(collector.buffer, "");
    }
}
