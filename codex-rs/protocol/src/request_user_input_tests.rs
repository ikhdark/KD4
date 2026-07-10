use super::*;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

#[test]
fn request_user_input_accepts_three_questions_and_preserves_optional_fields() {
    let args = serde_json::from_value::<RequestUserInputArgs>(json!({
        "questions": [
            question_json("q1", "One", 2),
            question_json("q2", "Two", 2),
            {
                "id": "q3",
                "header": "Three",
                "question": "Pick one",
                "isOther": true,
                "isSecret": true,
                "options": option_json(3)
            }
        ],
        "autoResolutionMs": 42
    }))
    .expect("three questions should deserialize");

    assert_eq!(args.questions.len(), 3);
    assert!(args.questions[2].is_other);
    assert!(args.questions[2].is_secret);
    assert_eq!(args.questions[2].options.as_ref().map(Vec::len), Some(3));
    assert_eq!(args.auto_resolution_ms, Some(42));
}

#[test]
fn request_user_input_rejects_question_count_outside_bounds() {
    for count in [0, 4] {
        let questions = (0..count)
            .map(|index| question_json(&format!("q{index}"), "Header", 2))
            .collect::<Vec<_>>();
        let err = serde_json::from_value::<RequestUserInputArgs>(json!({
            "questions": questions
        }))
        .expect_err("question count outside 1..=3 should fail");

        assert!(err.to_string().contains("requires 1 to 3 questions"));
    }
}

#[test]
fn request_user_input_counts_header_characters_not_bytes() {
    let twelve = "🦀".repeat(12);
    serde_json::from_value::<RequestUserInputArgs>(json!({
        "questions": [question_json("q1", &twelve, 2)]
    }))
    .expect("twelve Unicode characters should deserialize");

    let thirteen = "🦀".repeat(13);
    let err = serde_json::from_value::<RequestUserInputArgs>(json!({
        "questions": [question_json("q1", &thirteen, 2)]
    }))
    .expect_err("thirteen Unicode characters should fail");
    assert!(err.to_string().contains("12 characters or fewer"));
}

#[test]
fn request_user_input_accepts_two_or_three_options() {
    for count in [2, 3] {
        serde_json::from_value::<RequestUserInputArgs>(json!({
            "questions": [question_json("q1", "Mode", count)]
        }))
        .expect("two or three options should deserialize");
    }
}

#[test]
fn request_user_input_rejects_option_count_outside_bounds() {
    for count in [0, 1, 4] {
        let err = serde_json::from_value::<RequestUserInputArgs>(json!({
            "questions": [question_json("q1", "Mode", count)]
        }))
        .expect_err("option count outside 2..=3 should fail");

        assert!(err.to_string().contains("2 to 3 choices"));
    }
}

#[test]
fn request_user_input_leaves_missing_options_for_runtime_normalization() {
    let args = serde_json::from_value::<RequestUserInputArgs>(json!({
        "questions": [{
            "id": "q1",
            "header": "Mode",
            "question": "Pick one"
        }]
    }))
    .expect("the protocol keeps options optional");

    assert_eq!(args.questions[0].options, None);
    assert!(!args.questions[0].is_other);
    assert!(!args.questions[0].is_secret);
}

#[test]
fn request_user_input_rejects_unknown_root_fields() {
    let err = serde_json::from_value::<RequestUserInputArgs>(json!({
        "questions": [question_json("q1", "Mode", 2)],
        "autoResolveMs": 60_000
    }))
    .expect_err("unknown root field should fail");

    assert!(err.to_string().contains("unknown field"));
}

#[test]
fn request_user_input_rejects_unknown_question_fields() {
    let err = serde_json::from_value::<RequestUserInputArgs>(json!({
        "questions": [{
            "id": "q1",
            "header": "Mode",
            "question": "Pick one",
            "options": option_json(2),
            "queston": "typo"
        }]
    }))
    .expect_err("unknown question field should fail");

    assert!(err.to_string().contains("unknown field"));
}

#[test]
fn request_user_input_rejects_unknown_option_fields() {
    let err = serde_json::from_value::<RequestUserInputArgs>(json!({
        "questions": [{
            "id": "q1",
            "header": "Mode",
            "question": "Pick one",
            "options": [
                {"label": "A", "description": "Alpha", "descripton": "typo"},
                {"label": "B", "description": "Beta"}
            ]
        }]
    }))
    .expect_err("unknown option field should fail");

    assert!(err.to_string().contains("unknown field"));
}

#[test]
fn request_user_input_event_remains_forward_compatible() {
    let event = serde_json::from_value::<RequestUserInputEvent>(json!({
        "call_id": "call-1",
        "turn_id": "turn-1",
        "questions": [{
            "id": "q1",
            "header": "Mode",
            "question": "Pick one",
            "futureQuestionField": true,
            "options": [
                {"label": "A", "description": "Alpha", "futureOptionField": true},
                {"label": "B", "description": "Beta"}
            ]
        }],
        "futureEventField": true
    }))
    .expect("shared events should continue ignoring future fields");

    assert_eq!(event.questions.len(), 1);
    assert_eq!(event.questions[0].options.as_ref().map(Vec::len), Some(2));
}

fn question_json(id: &str, header: &str, option_count: usize) -> Value {
    json!({
        "id": id,
        "header": header,
        "question": "Pick one",
        "options": option_json(option_count)
    })
}

fn option_json(count: usize) -> Vec<Value> {
    (0..count)
        .map(|index| {
            json!({
                "label": format!("Option {index}"),
                "description": format!("Description {index}")
            })
        })
        .collect()
}
