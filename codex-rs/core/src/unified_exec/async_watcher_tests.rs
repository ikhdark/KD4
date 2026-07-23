use super::append_output_loss_markers;
use super::lagged_output_marker;
use super::omitted_output_marker;
use super::resolve_aggregated_output;
use super::split_valid_utf8_prefix_with_max;

use pretty_assertions::assert_eq;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::unified_exec::head_tail_buffer::HeadTailBuffer;

#[test]
fn split_valid_utf8_prefix_respects_max_bytes_for_ascii() {
    let mut buf = b"hello word!".to_vec();

    let first = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 5, /*flush_incomplete*/ false,
    )
    .expect("expected prefix");
    assert_eq!(first, b"hello".to_vec());
    assert_eq!(buf, b" word!".to_vec());

    let second = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 5, /*flush_incomplete*/ false,
    )
    .expect("expected prefix");
    assert_eq!(second, b" word".to_vec());
    assert_eq!(buf, b"!".to_vec());
}

#[test]
fn split_valid_utf8_prefix_avoids_splitting_utf8_codepoints() {
    // "é" is 2 bytes in UTF-8. With a max of 3 bytes, we should only emit 1 char (2 bytes).
    let mut buf = "ééé".as_bytes().to_vec();

    let first = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 3, /*flush_incomplete*/ false,
    )
    .expect("expected prefix");
    assert_eq!(std::str::from_utf8(&first).unwrap(), "é");
    assert_eq!(buf, "éé".as_bytes().to_vec());
}

#[test]
fn split_valid_utf8_prefix_makes_progress_on_invalid_utf8() {
    let mut buf = vec![0xff, b'a', b'b'];

    let first = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 2, /*flush_incomplete*/ false,
    )
    .expect("expected prefix");
    assert_eq!(first, vec![0xff]);
    assert_eq!(buf, b"ab".to_vec());
}

#[test]
fn split_valid_utf8_prefix_waits_for_a_codepoint_split_across_chunks() {
    let mut buf = vec![0xc3];

    assert_eq!(
        split_valid_utf8_prefix_with_max(
            &mut buf, /*max_bytes*/ 8, /*flush_incomplete*/ false,
        ),
        None
    );
    assert_eq!(buf, vec![0xc3]);

    buf.push(0xa9);

    let completed = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 8, /*flush_incomplete*/ false,
    )
    .expect("expected completed code point");
    assert_eq!(completed, "é".as_bytes());
    assert!(buf.is_empty());
}

#[test]
fn split_valid_utf8_prefix_flushes_permanently_incomplete_bytes_at_end_of_stream() {
    let mut buf = vec![0xe2, 0x82];

    let first = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 8, /*flush_incomplete*/ true,
    )
    .expect("expected first incomplete byte");
    let second = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 8, /*flush_incomplete*/ true,
    )
    .expect("expected second incomplete byte");

    assert_eq!(first, vec![0xe2]);
    assert_eq!(second, vec![0x82]);
    assert!(buf.is_empty());
}

#[test]
fn lagged_output_is_explicit_in_the_transcript() {
    assert_eq!(
        String::from_utf8(lagged_output_marker(7)).expect("marker is UTF-8"),
        "\n[output unavailable: streaming receiver lagged by 7 chunk(s)]\n"
    );
}

#[test]
fn capacity_omission_is_distinct_from_broadcast_lag() {
    assert_eq!(
        String::from_utf8(omitted_output_marker(64)).expect("marker is UTF-8"),
        "\n[output truncated: 64 byte(s) omitted from the middle by the output retention limit]\n"
    );
}

#[test]
fn finalization_does_not_duplicate_existing_loss_markers() {
    let output = format!(
        "prefix{}{}",
        String::from_utf8(omitted_output_marker(64)).expect("omission marker"),
        String::from_utf8(lagged_output_marker(7)).expect("lag marker")
    );

    let finalized = append_output_loss_markers(output, 64, 7);

    assert_eq!(
        finalized
            .matches("64 byte(s) omitted from the middle")
            .count(),
        1
    );
    assert_eq!(
        finalized
            .matches("streaming receiver lagged by 7 chunk(s)")
            .count(),
        1
    );
}

#[tokio::test]
async fn final_loss_markers_survive_head_tail_eviction_without_duplication() {
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::new(16)));
    {
        let mut guard = transcript.lock().await;
        guard.push_chunk(vec![b'a'; 16]);
        guard.record_lagged_chunks(7);
        guard.push_chunk(vec![b'b'; 64]);
    }

    let aggregated = resolve_aggregated_output(&transcript, String::new()).await;

    assert_eq!(
        aggregated
            .matches("64 byte(s) omitted from the middle")
            .count(),
        1
    );
    assert_eq!(
        aggregated
            .matches("streaming receiver lagged by 7 chunk(s)")
            .count(),
        1
    );
    assert!(aggregated.contains("bbbbbbbb"));
}

#[tokio::test]
async fn final_capacity_marker_separates_nonadjacent_head_and_tail() {
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::new(8)));
    transcript.lock().await.push_chunk(b"pass---word".to_vec());

    let aggregated = resolve_aggregated_output(&transcript, String::new()).await;

    assert_eq!(
        aggregated,
        format!(
            "pass{}word",
            String::from_utf8(omitted_output_marker(3)).expect("marker is UTF-8")
        )
    );
    assert!(!aggregated.contains("password"));
}
