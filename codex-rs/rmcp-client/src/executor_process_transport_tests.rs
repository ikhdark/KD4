use bytes::BytesMut;
use pretty_assertions::assert_eq;

use super::LineBuffer;

const TEST_LINE_LIMIT: usize = 64;

#[test]
fn searches_only_new_bytes_after_partial_line() {
    let mut buffer = LineBuffer::default();

    buffer
        .extend_from_slice(b"partial", TEST_LINE_LIMIT)
        .unwrap();
    assert_eq!(buffer.take_line(), None);
    assert_eq!(
        buffer,
        LineBuffer {
            bytes: BytesMut::from(&b"partial"[..]),
            scanned_len: 7,
            trailing_line_len: 7,
        }
    );

    buffer.extend_from_slice(b" line", TEST_LINE_LIMIT).unwrap();
    assert_eq!(buffer.take_line(), None);
    assert_eq!(
        buffer,
        LineBuffer {
            bytes: BytesMut::from(&b"partial line"[..]),
            scanned_len: 12,
            trailing_line_len: 12,
        }
    );

    buffer
        .extend_from_slice(b"\nnext", TEST_LINE_LIMIT)
        .unwrap();
    assert_eq!(
        buffer.take_line(),
        Some(BytesMut::from(&b"partial line"[..]))
    );
    assert_eq!(
        buffer,
        LineBuffer {
            bytes: BytesMut::from(&b"next"[..]),
            scanned_len: 0,
            trailing_line_len: 4,
        }
    );
}

#[test]
fn splits_multiple_lines_and_retains_partial_tail() {
    let mut buffer = LineBuffer::default();
    buffer
        .extend_from_slice(b"first\nsecond\npartial", TEST_LINE_LIMIT)
        .unwrap();

    assert_eq!(buffer.take_line(), Some(BytesMut::from(&b"first"[..])));
    assert_eq!(buffer.take_line(), Some(BytesMut::from(&b"second"[..])));
    assert_eq!(buffer.take_line(), None);
    assert_eq!(
        buffer,
        LineBuffer {
            bytes: BytesMut::from(&b"partial"[..]),
            scanned_len: 7,
            trailing_line_len: 7,
        }
    );
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
    assert_eq!(buffer, LineBuffer::default());
}

#[test]
fn rejects_an_oversized_unterminated_line_without_growing() {
    let mut buffer = LineBuffer::default();
    let at_limit = vec![b'x'; TEST_LINE_LIMIT];
    buffer
        .extend_from_slice(&at_limit, TEST_LINE_LIMIT)
        .unwrap();

    assert!(buffer.extend_from_slice(b"x", TEST_LINE_LIMIT).is_err());
    assert_eq!(buffer.bytes.len(), TEST_LINE_LIMIT);
    assert_eq!(buffer.trailing_line_len, TEST_LINE_LIMIT);
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
    assert_eq!(buffer, LineBuffer::default());
}
