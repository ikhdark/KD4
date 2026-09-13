use super::HeadTailBuffer;

use pretty_assertions::assert_eq;

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
fn max_bytes_zero_drops_everything() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 0);
    buf.push_chunk(b"abc");

    assert_eq!(buf.retained_bytes(), 0);
    assert_eq!(buf.omitted_bytes(), 3);
    assert_eq!(buf.to_bytes(), b"".to_vec());
    assert_eq!(buf.snapshot_chunks(), Vec::<Vec<u8>>::new());
}

#[test]
fn head_budget_zero_keeps_only_last_byte_in_tail() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 1);
    buf.push_chunk(b"abc");

    assert_eq!(buf.retained_bytes(), 1);
    assert_eq!(buf.omitted_bytes(), 2);
    assert_eq!(buf.to_bytes(), b"c".to_vec());
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
        buf.push_chunk(&vec![*byte]);
    }

    assert_eq!(
        buf.snapshot_chunks(),
        vec![b"01234".to_vec(), b"789ab".to_vec()]
    );
    assert_eq!(buf.retained_bytes(), 10);
    assert_eq!(buf.omitted_bytes(), 2);
}
