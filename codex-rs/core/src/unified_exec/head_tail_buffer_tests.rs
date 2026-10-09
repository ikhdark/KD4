use super::HeadTailBuffer;

use pretty_assertions::assert_eq;

#[test]
fn utf8_projection_is_independent_of_poll_boundaries_and_retry() {
    for input in ["aé中😀z".as_bytes(), b"a\xff\xe2\x82\xac\xe2\x82"] {
        for split in 0..=input.len() {
            let mut buffer = HeadTailBuffer::default();
            buffer.push_chunk(&input[..split]);
            buffer.collect_pending_output();
            let first = buffer.projected_pending_output(false, &[]);
            assert_eq!(first, buffer.projected_pending_output(false, &[]));
            buffer.acknowledge_pending_output();
            buffer.push_chunk(&input[split..]);
            buffer.collect_pending_output();
            let second = buffer.projected_pending_output(true, &[]);
            let text = format!("{}{}", String::from_utf8_lossy(&first.0), String::from_utf8_lossy(&second.0));
            assert_eq!(text, String::from_utf8_lossy(input));
            let first_ranges = first.1.unwrap();
            let second_ranges = second.1.unwrap();
            assert_eq!(first_ranges.range.start, 0);
            assert_eq!(first_ranges.range.end, second_ranges.range.start);
            assert_eq!(second_ranges.range.end, input.len() as u64);
            assert_eq!(first_ranges.gap, None);
            assert_eq!(second_ranges.gap, None);
            buffer.acknowledge_pending_output();
            assert!(!buffer.has_unreported_output());
        }
    }
}

#[test]
fn utf8_projection_carries_every_byte_of_a_multibyte_character() {
    let mut buffer = HeadTailBuffer::default();
    let mut projected = String::new();
    for byte in "é中😀".as_bytes() {
        buffer.push_chunk(&[*byte]);
        buffer.collect_pending_output();
        let (bytes, _) = buffer.projected_pending_output(false, &[]);
        projected.push_str(&String::from_utf8_lossy(&bytes));
        buffer.acknowledge_pending_output();
    }
    assert_eq!(projected, "é中😀");
}

#[test]
fn pending_reports_keep_absolute_chunk_and_gap_coordinates() {
    let mut source = HeadTailBuffer::new(8);
    source.push_chunk(b"first\r\n");
    source.collect_pending_output();
    assert_eq!(source.pending_output().unwrap().output_ranges().unwrap().range, 0..7);
    source.acknowledge_pending_output();
    source.push_chunk(b"abcdefghijklmnop");
    source.collect_pending_output();
    let ranges = source.pending_output().unwrap().output_ranges().unwrap();
    assert_eq!(ranges.range, 7..23);
    assert_eq!(ranges.gap, Some(11..19));
    // A cancelled preparation retains the same in-flight report. New producer
    // bytes extend it without moving its starting coordinate or replaying reads.
    source.begin_output_report();
    source.collect_pending_output();
    assert_eq!(source.pending_output().unwrap().output_ranges(), Some(ranges));
    source.push_chunk(b"tail");
    source.collect_pending_output();
    assert_eq!(source.pending_output().unwrap().output_ranges().unwrap().range, 7..27);
    source.acknowledge_pending_output();
    source.collect_pending_output();
    let empty = source.pending_output().unwrap().output_ranges().unwrap();
    assert_eq!(empty.range, 27..27);
    assert_eq!(empty.gap, None);
    source.acknowledge_pending_output();
    source.record_lagged_chunks(1);
    source.push_chunk(b"unknown offset");
    source.collect_pending_output();
    assert_eq!(source.pending_output().unwrap().output_ranges(), None);
    let mut notices = HeadTailBuffer::default();
    notices.push_chunk(b"producer");
    notices.push_display_notice(b"not in artifact");
    notices.collect_pending_output();
    assert_eq!(notices.pending_output().unwrap().output_ranges(), None);
}

#[test]
fn loss_notice_snapshot_preserves_wrapped_tail_and_trailing_notice() {
    let mut buf = HeadTailBuffer::new(8);
    buf.push_chunk(b"abcd");
    // Force two tail slices so neither half can be lost during materialization.
    buf.tail = std::collections::VecDeque::with_capacity(4);
    buf.tail.extend(b"efgh");
    drop(buf.tail.drain(..2));
    buf.tail.extend(b"ij");
    buf.record_omitted_bytes(2);
    assert!(!buf.tail.as_slices().1.is_empty());

    assert_eq!(buf.to_bytes(), b"abcdghij");
    assert_eq!(
        buf.to_bytes_with_omission_marker(b"<gap>"),
        b"abcd<gap>ghij"
    );
    assert_eq!(
        buf.to_bytes_with_loss_notice(b"<lag>"),
        b"abcd\n[output truncated: 2 byte(s) omitted from the middle by the output retention limit]\nghij<lag>"
    );

    let mut complete = HeadTailBuffer::new(8);
    complete.push_chunk(b"complete");
    assert_eq!(complete.to_bytes_with_loss_notice(&[]), b"complete");
    assert_eq!(complete.to_bytes_with_loss_notice(b"<lag>"), b"complete<lag>");
    assert_eq!(HeadTailBuffer::new(0).to_bytes_with_loss_notice(&[]), b"");
}

#[test]
fn keeps_prefix_and_suffix_when_over_budget() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);

    buf.push_chunk(b"0123456789");
    assert_eq!(buf.omitted_bytes(), 0);

    // Exceeds max by 2; we should keep head+tail and omit the middle.
    buf.push_chunk(b"ab");
    assert_eq!(buf.omitted_bytes(), 2);
    assert_eq!(buf.to_bytes(), b"01234789ab");
}

#[test]
fn zero_head_budget_respects_zero_and_one_byte_capacity() {
    for (capacity, expected) in [(0, b"".as_slice()), (1, b"c".as_slice())] {
        let mut buf = HeadTailBuffer::new(capacity);
        buf.push_chunk(b"abc");

        assert_eq!(buf.retained_bytes(), capacity);
        assert_eq!(buf.omitted_bytes(), 3 - capacity);
        assert_eq!(buf.to_bytes(), expected);
        let chunks = if capacity == 0 { Vec::new() } else { vec![expected.to_vec()] };
        assert_eq!(buf.snapshot_chunks(), chunks);
        assert_eq!(buf.take_unreported_omitted_bytes(), 3 - capacity);
        assert_eq!(buf.take_unreported_omitted_bytes(), 0);
    }
}

#[test]
fn draining_resets_bytes_but_preserves_cumulative_loss_accounting() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);
    buf.push_chunk(b"0123456789");
    buf.push_chunk(b"ab");
    buf.record_lagged_chunks(3);

    let drained = buf.drain_chunks();
    assert_eq!(drained, vec![b"01234".to_vec(), b"789ab".to_vec()]);

    assert_eq!(buf.retained_bytes(), 0);
    assert_eq!(buf.omitted_bytes(), 2);
    assert_eq!(buf.take_unreported_omitted_bytes(), 2);
    assert_eq!(buf.take_unreported_omitted_bytes(), 0);
    assert_eq!(buf.omitted_bytes(), 2);
    assert_eq!(buf.lagged_chunks(), 3);
    assert_eq!(buf.take_unreported_lagged_chunks(), 3);
    assert_eq!(buf.take_unreported_lagged_chunks(), 0);
    assert_eq!(buf.lagged_chunks(), 3);
    assert_eq!(buf.to_bytes(), b"".to_vec());
    buf.push_chunk(b"ABCDEFGHIJKL");
    assert_eq!(buf.to_bytes(), b"ABCDEHIJKL");
    assert_eq!(buf.omitted_bytes(), 4);
    assert_eq!(buf.take_unreported_omitted_bytes(), 2);
}

#[test]
fn chunk_larger_than_tail_budget_keeps_only_tail_end() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);
    buf.push_chunk(b"0123456789");

    // Tail budget is 5 bytes. This chunk should replace the tail and keep only its last 5 bytes.
    buf.push_chunk(b"ABCDEFGHIJK");

    assert_eq!(buf.to_bytes(), b"01234GHIJK");
    assert_eq!(buf.omitted_bytes(), 11);
}

#[test]
fn bounded_drain_preserves_source_gaps_and_exact_combined_loss() {
    let mut source = HeadTailBuffer::new(8);
    let mut response = HeadTailBuffer::new(12);
    source.push_chunk(b"pass---word");
    assert!(source.drain_into(&mut response));
    assert_eq!(
        response.to_bytes_with_omission_marker(b"<gap>"),
        b"pass<gap>word"
    );
    assert_eq!(response.omitted_bytes(), 3);
    assert!(!source.has_unreported_output());

    source.push_chunk(b"0123456789abcdef");
    assert!(source.drain_into(&mut response));
    assert_eq!(
        response.to_bytes_with_omission_marker(b"<gap>"),
        b"pass<gap>cdef"
    );
    assert_eq!(response.omitted_bytes(), 19);
    assert_eq!(source.omitted_bytes(), 11);

    source.push_chunk(b"GHIJKLMN");
    source.drain_into(&mut response);
    assert_eq!(
        response.to_bytes_with_omission_marker(b"<gap>"),
        b"pass<gap>IJKLMN"
    );
    assert_eq!(response.omitted_bytes(), 25);
}

#[test]
fn fills_head_then_tail_across_multiple_chunks() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);

    // Fill the 5-byte head budget across multiple chunks.
    buf.push_chunk(b"01");
    buf.push_chunk(b"234");
    assert_eq!(buf.to_bytes(), b"01234".to_vec());

    // Then fill the 5-byte tail budget.
    buf.push_chunk(b"567");
    buf.push_chunk(b"89");
    assert_eq!(buf.to_bytes(), b"0123456789".to_vec());
    assert_eq!(buf.omitted_bytes(), 0);

    // One more byte causes the tail to drop its oldest byte.
    buf.push_chunk(b"a");
    assert_eq!(buf.to_bytes(), b"012346789a".to_vec());
    assert_eq!(buf.omitted_bytes(), 1);
}

#[test]
fn empty_and_tiny_chunks_have_bounded_metadata() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);

    for byte in b"0123456789ab" {
        buf.push_chunk(&[]);
        buf.push_chunk(&[*byte]);
    }

    assert_eq!(
        buf.snapshot_chunks(),
        vec![b"01234".to_vec(), b"789ab".to_vec()]
    );
    assert_eq!(buf.retained_bytes(), 10);
    assert_eq!(buf.omitted_bytes(), 2);
}
