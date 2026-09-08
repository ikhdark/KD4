use bytes::BytesMut;
use pretty_assertions::assert_eq;

use super::LineBuffer;

const TEST_LINE_LIMIT: usize = 64;

#[test]
fn fragmented_lines_preserve_order_and_the_unterminated_tail() {
    let mut buffer = LineBuffer::default();

    buffer
        .extend_from_slice(b"partial", TEST_LINE_LIMIT)
        .unwrap();
    assert_eq!(buffer.take_line(), None);

    buffer.extend_from_slice(b" line", TEST_LINE_LIMIT).unwrap();
    assert_eq!(buffer.take_line(), None);

    buffer
        .extend_from_slice(b"\nnext\npartial", TEST_LINE_LIMIT)
        .unwrap();
    assert_eq!(
        buffer.take_line(),
        Some(BytesMut::from(&b"partial line"[..]))
    );
    assert_eq!(buffer.take_line(), Some(BytesMut::from(&b"next"[..])));
    assert_eq!(buffer.take_line(), None);
    assert_eq!(
        buffer.take_remaining(),
        Some(BytesMut::from(&b"partial"[..]))
    );
    assert_eq!(buffer.take_remaining(), None);
}

#[test]
fn takes_unterminated_remaining_bytes_at_eof() {
    let mut buffer = LineBuffer::default();
    buffer
        .extend_from_slice(b"remaining", TEST_LINE_LIMIT)
        .unwrap();
    assert_eq!(buffer.take_line(), None);

    assert_eq!(
        buffer.take_remaining(),
        Some(BytesMut::from(&b"remaining"[..]))
    );
    assert_eq!(buffer.take_line(), None);
    assert_eq!(buffer.take_remaining(), None);

    let at_limit = vec![b'x'; TEST_LINE_LIMIT];
    buffer
        .extend_from_slice(&at_limit, TEST_LINE_LIMIT)
        .unwrap();
    buffer.extend_from_slice(b"\n", TEST_LINE_LIMIT).unwrap();
    assert_eq!(
        buffer.take_line(),
        Some(BytesMut::from(at_limit.as_slice()))
    );
    assert_eq!(buffer.take_remaining(), None);
}

#[test]
fn rejected_oversized_append_preserves_the_accepted_line() {
    let mut buffer = LineBuffer::default();
    let at_limit = vec![b'x'; TEST_LINE_LIMIT];
    buffer
        .extend_from_slice(&at_limit, TEST_LINE_LIMIT)
        .unwrap();

    assert!(buffer.extend_from_slice(b"x", TEST_LINE_LIMIT).is_err());
    assert_eq!(buffer.take_line(), None);
    buffer.extend_from_slice(b"\n", TEST_LINE_LIMIT).unwrap();
    assert_eq!(
        buffer.take_line(),
        Some(BytesMut::from(at_limit.as_slice()))
    );
    assert_eq!(buffer.take_remaining(), None);
}

#[test]
fn bounds_each_line_instead_of_the_aggregate_buffer() {
    let mut buffer = LineBuffer::default();
    let lines = b"first line\nsecond line\nthird line\n";

    buffer
        .extend_from_slice(lines, b"second line".len())
        .unwrap();

    assert_eq!(buffer.take_line(), Some(BytesMut::from(&b"first line"[..])));
    assert_eq!(
        buffer.take_line(),
        Some(BytesMut::from(&b"second line"[..]))
    );
    assert_eq!(buffer.take_line(), Some(BytesMut::from(&b"third line"[..])));
    assert_eq!(buffer.take_line(), None);
    assert_eq!(buffer.take_remaining(), None);
}
