use super::LIVE_COMMAND_OUTPUT_LINE_HEAD_BYTES;
use super::LIVE_COMMAND_OUTPUT_LINE_TAIL_BYTES;
use super::LIVE_COMMAND_OUTPUT_MAX_BYTES;
use super::LiveCommandOutput;
use crate::ansi_escape::ansi_escape_line;
use pretty_assertions::assert_eq;

#[test]
fn keeps_all_short_lines_and_chunk_boundaries_within_the_live_byte_budget() {
    let mut output = LiveCommandOutput::default();
    for line in 1..=500 {
        output.push_str(&format!("line {line}\n"));
    }
    for chunk in ["hell", "o\r", "\n\nwor", "ld"] {
        output.push_str(chunk);
    }

    let expected: Vec<_> = (1..=500)
        .map(|line| format!("line {line}"))
        .chain(["hello".to_string(), String::new(), "world".to_string()])
        .collect();

    assert_eq!(output.total_lines(), expected.len());
    assert_eq!(output.retained_lines(), expected.len());
    assert_eq!(
        output.transcript_lines().collect::<Vec<_>>(),
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
}

#[test]
fn switches_to_bounded_storage_after_the_byte_budget_and_preserves_split_crlf() {
    let mut output = LiveCommandOutput::default();
    let line = "x".repeat(LIVE_COMMAND_OUTPUT_MAX_BYTES);
    output.push_str(&line);
    assert_eq!(output.transcript_lines().next().expect("full line"), line);

    output.push_str("y");
    let line = output.transcript_lines().next().expect("bounded line");
    assert!(line.contains("bytes omitted"));
    assert!(line.ends_with("xy"));

    for carriage_returns in ["\r", "\r\r"] {
        let body = "x".repeat(LIVE_COMMAND_OUTPUT_MAX_BYTES - carriage_returns.len());
        let mut contiguous = LiveCommandOutput::default();
        contiguous.push_str(&format!("{body}{carriage_returns}\n"));

        let mut split = LiveCommandOutput::default();
        split.push_str(&body);
        split.push_str(carriage_returns);
        assert!(
            split
                .transcript_lines()
                .next()
                .expect("partial line")
                .ends_with('\r')
        );
        split.push_str("\n");

        assert_eq!(split.total_lines(), 1);
        assert_eq!(split.retained_lines(), 1);
        assert_eq!(
            split.lines().collect::<Vec<_>>(),
            contiguous.lines().collect::<Vec<_>>()
        );
        // Only the CRLF terminator is consumed; an earlier lone CR stays in the line.
        assert_eq!(
            split
                .lines()
                .next()
                .expect("completed line")
                .ends_with('\r'),
            carriage_returns == "\r\r"
        );
    }
}

#[test]
fn truncated_ansi_sequence_does_not_hide_the_retained_tail() {
    let mut output = LiveCommandOutput::default();
    let prefix = "x".repeat(LIVE_COMMAND_OUTPUT_LINE_HEAD_BYTES - 2);
    let osc = format!("{prefix}\x1b]0;{}\x07visible-tail", "hidden".repeat(4_000));
    output.push_str(&osc);

    let line = output.lines().next().expect("partial line");
    let rendered = ansi_escape_line(line.as_ref())
        .spans
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect::<String>();

    assert!(rendered.contains("bytes omitted"), "{rendered}");
    assert!(rendered.ends_with("visible-tail"), "{rendered}");
    assert_eq!(
        output.transcript_lines().next().expect("transcript line"),
        osc
    );
}

#[test]
fn complete_csi_sequences_do_not_gain_visible_terminators_when_truncated() {
    // CSI final bytes include punctuation, not just alphabetic characters.
    for final_byte in ['@', '~'] {
        let head = format!(
            "{}\x1b[1{final_byte}",
            "x".repeat(LIVE_COMMAND_OUTPUT_LINE_HEAD_BYTES - 4)
        );
        let mut output = LiveCommandOutput::default();
        output.push_str(&format!("{head}{}tail", "y".repeat(20_000)));
        let line = output.lines().next().expect("truncated preview");

        assert!(line.contains("bytes omitted"));
        assert!(line.ends_with("tail"));
        // The complete source escape needs no synthetic final character before
        // the reset; appending 'm' here would insert visible text in the output.
        assert!(line.starts_with(&format!("{head}\x1b[0m... ")));
    }
}

#[test]
fn bounds_long_no_newline_output_and_preserves_utf8_head_and_tail() {
    let mut output = LiveCommandOutput::default();
    // Both byte cuts must land inside a character, or the boundary handling goes untested.
    assert_ne!(LIVE_COMMAND_OUTPUT_LINE_HEAD_BYTES % "界".len(), 0);
    assert_ne!(LIVE_COMMAND_OUTPUT_LINE_TAIL_BYTES % "界".len(), 0);
    let chunk = "界".repeat(1024);
    for _ in 0..600 {
        output.push_str(&chunk);
    }

    let line = output.lines().next().expect("partial line");
    let retained_bytes = LIVE_COMMAND_OUTPUT_LINE_HEAD_BYTES + LIVE_COMMAND_OUTPUT_LINE_TAIL_BYTES
        - LIVE_COMMAND_OUTPUT_LINE_HEAD_BYTES % "界".len()
        - LIVE_COMMAND_OUTPUT_LINE_TAIL_BYTES % "界".len();

    assert_eq!(output.total_lines(), 1);
    assert_eq!(output.retained_lines(), 1);
    assert!(line.starts_with("界界界"));
    assert!(line.ends_with("界界界"));
    assert!(line.contains(&format!(
        "... {} bytes omitted ...",
        600 * chunk.len() - retained_bytes
    )));
    assert!(line.len() < LIVE_COMMAND_OUTPUT_MAX_BYTES);
}

#[test]
fn retained_output_stays_within_the_live_byte_budget() {
    let mut output = LiveCommandOutput::default();
    let body = "output ".repeat(4_000);
    for line in 1..=180 {
        output.push_str(&format!("head-{line} {body} tail-{line}\n"));
    }
    output.push_str(&format!("partial-head {body} partial-tail"));

    let lines: Vec<_> = output.lines().collect();
    assert_eq!(output.total_lines(), 181);
    assert_eq!(output.retained_lines(), 101);
    assert_eq!(lines.len(), 101);
    for (line, number) in lines.iter().zip((1..=50).chain(131..=180)) {
        assert!(line.starts_with(&format!("head-{number} ")), "{number}");
        assert!(line.ends_with(&format!(" tail-{number}")), "{number}");
    }
    assert_eq!(output.lines().rev().collect::<Vec<_>>(), lines.iter().rev().cloned().collect::<Vec<_>>());
    assert!(lines.last().expect("tail line").ends_with(" partial-tail"));
    assert_eq!(
        output.transcript_lines().nth(50).expect("omission line"),
        "… +80 lines"
    );
    assert!(lines.iter().map(|line| line.len()).sum::<usize>() <= LIVE_COMMAND_OUTPUT_MAX_BYTES);
}
