use super::*;

#[test]
fn byte_range_accepts_utf8_boundaries() {
    ByteRange::new(0, "\u{00e9}".len())
        .validate_for_text("\u{00e9}")
        .expect("valid UTF-8 byte range should pass");
}

#[test]
fn byte_range_rejects_reversed_offsets() {
    let err = ByteRange::new(2, 1)
        .validate_for_text("abc")
        .expect_err("reversed offsets should fail");

    assert!(err.contains("less than or equal"));
}

#[test]
fn byte_range_rejects_mid_codepoint_offsets() {
    let err = ByteRange::new(0, 1)
        .validate_for_text("\u{00e9}")
        .expect_err("mid-codepoint end should fail");

    assert!(err.contains("character boundary"));
}

#[test]
fn text_element_validation_reports_range_errors() {
    let err = TextElement::new(ByteRange::new(4, 5), None)
        .validate_for_text("abc")
        .expect_err("out-of-range element should fail");

    assert!(err.contains("text element range"));
}

#[test]
fn user_input_validation_reports_nested_text_element_errors() {
    let err = UserInput::Text {
        text: "abc".to_string(),
        text_elements: vec![TextElement::new(ByteRange::new(4, 5), None)],
    }
    .validate()
    .expect_err("invalid text element should fail");

    assert!(err.contains("text element 0"));
}
