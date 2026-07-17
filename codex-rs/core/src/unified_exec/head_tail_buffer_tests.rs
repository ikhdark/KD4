use super::HeadTailBuffer;

use pretty_assertions::assert_eq;

#[test]
fn keeps_prefix_and_suffix_when_over_budget() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);

    buf.push_chunk(b"0123456789".to_vec());
    assert_eq!(buf.omitted_bytes(), 0);

    // Exceeds max by 2; we should keep head+tail and omit the middle.
    buf.push_chunk(b"ab".to_vec());
    assert!(buf.omitted_bytes() > 0);

    let rendered = String::from_utf8_lossy(&buf.to_bytes()).to_string();
    assert!(rendered.starts_with("01234"));
    assert!(rendered.ends_with("89ab"));
}

#[test]
fn max_bytes_zero_drops_everything() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 0);
    buf.push_chunk(b"abc".to_vec());

    assert_eq!(buf.retained_bytes(), 0);
    assert_eq!(buf.omitted_bytes(), 3);
    assert_eq!(buf.to_bytes(), b"".to_vec());
    assert_eq!(buf.snapshot_chunks(), Vec::<Vec<u8>>::new());
}

#[test]
fn head_budget_zero_keeps_only_last_byte_in_tail() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 1);
    buf.push_chunk(b"abc".to_vec());

    assert_eq!(buf.retained_bytes(), 1);
    assert_eq!(buf.omitted_bytes(), 2);
    assert_eq!(buf.to_bytes(), b"c".to_vec());
}

#[test]
fn draining_resets_bytes_but_preserves_cumulative_loss_accounting() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);
    buf.push_chunk(b"0123456789".to_vec());
    buf.push_chunk(b"ab".to_vec());
    buf.record_lagged_chunks(3);

    let drained = buf.drain_chunks();
    assert!(!drained.is_empty());

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
}

#[test]
fn chunk_larger_than_tail_budget_keeps_only_tail_end() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);
    buf.push_chunk(b"0123456789".to_vec());

    // Tail budget is 5 bytes. This chunk should replace the tail and keep only its last 5 bytes.
    buf.push_chunk(b"ABCDEFGHIJK".to_vec());

    let out = String::from_utf8_lossy(&buf.to_bytes()).to_string();
    assert!(out.starts_with("01234"));
    assert!(out.ends_with("GHIJK"));
    assert!(buf.omitted_bytes() > 0);
}

#[test]
fn fills_head_then_tail_across_multiple_chunks() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);

    // Fill the 5-byte head budget across multiple chunks.
    buf.push_chunk(b"01".to_vec());
    buf.push_chunk(b"234".to_vec());
    assert_eq!(buf.to_bytes(), b"01234".to_vec());

    // Then fill the 5-byte tail budget.
    buf.push_chunk(b"567".to_vec());
    buf.push_chunk(b"89".to_vec());
    assert_eq!(buf.to_bytes(), b"0123456789".to_vec());
    assert_eq!(buf.omitted_bytes(), 0);

    // One more byte causes the tail to drop its oldest byte.
    buf.push_chunk(b"a".to_vec());
    assert_eq!(buf.to_bytes(), b"012346789a".to_vec());
    assert_eq!(buf.omitted_bytes(), 1);
}

#[test]
fn one_byte_chunks_use_bounded_storage_and_preserve_exact_head_and_tail() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 1_024);

    for index in 0..100_000_u32 {
        buf.push_chunk(vec![(index % 251) as u8]);
    }

    let retained = buf.to_bytes();
    let expected_head = (0..512_u32)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let expected_tail = (100_000_u32 - 512..100_000_u32)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    assert_eq!(buf.retained_bytes(), 1_024);
    assert_eq!(buf.omitted_bytes(), 100_000 - 1_024);
    assert_eq!(&retained[..512], expected_head);
    assert_eq!(&retained[512..], expected_tail);
    assert!(buf.snapshot_chunks().len() <= super::MAX_RETAINED_CHUNKS);
}

#[test]
fn circular_tail_wraps_across_both_storage_segments() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 10);
    buf.push_chunk(b"0123456789".to_vec());
    buf.push_chunk(b"abc".to_vec());
    buf.push_chunk(b"defg".to_vec());

    assert_eq!(buf.to_bytes(), b"01234cdefg".to_vec());
    assert_eq!(buf.omitted_bytes(), 7);
}

#[test]
fn rendered_marker_reserves_its_actual_length_and_preserves_head_and_tail() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 100);
    buf.push_chunk([vec![b'H'; 100], vec![b'T'; 100]].concat());

    let rendered = buf.render_bytes();

    assert_eq!(rendered.len(), 100);
    assert!(rendered.starts_with(b"H"));
    assert!(rendered.ends_with(b"T"));
    let text = String::from_utf8(rendered).expect("marker is UTF-8");
    assert_eq!(text.matches("output truncated").count(), 1);
}

#[test]
fn rendered_marker_uses_only_its_prefix_when_the_cap_is_tiny() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 8);
    buf.push_chunk(b"0123456789".to_vec());

    assert_eq!(buf.render_bytes(), b"\n[output".to_vec());
}

#[test]
fn rendered_output_without_evidence_is_exact() {
    let mut buf = HeadTailBuffer::new(/*max_bytes*/ 64);
    buf.push_chunk(b"exact output".to_vec());

    assert_eq!(buf.render_bytes(), b"exact output".to_vec());
}

#[test]
fn recovery_evidence_is_shared_and_reported_once_per_buffer() {
    let evidence = std::sync::Arc::new(std::sync::Mutex::new(Default::default()));
    let mut first = HeadTailBuffer::new_with_recovery_evidence(128, evidence.clone());
    let second = HeadTailBuffer::new_with_recovery_evidence(128, evidence);

    assert!(first.record_recovery_detail("missing sequences 2-4, 8-9".to_string()));
    assert_eq!(
        first.take_unreported_recovery_detail().as_deref(),
        Some("missing sequences 2-4, 8-9")
    );
    assert_eq!(first.take_unreported_recovery_detail(), None);
    assert!(
        String::from_utf8(second.render_bytes())
            .expect("rendered output")
            .contains("missing sequences 2-4, 8-9")
    );
}
