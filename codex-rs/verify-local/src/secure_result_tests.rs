use super::*;
use crate::model::CommandResultV2;
use crate::model::LogState;

fn result() -> CommandResultV2 {
    CommandResultV2 {
        schema_version: 2,
        invocation_id: "0123456789abcdef0123456789abcdef".to_string(),
        command_id: "owner:test/with:separators".to_string(),
        command_ordinal: 7,
        runner_nonce: "abcdef0123456789abcdef0123456789".to_string(),
        exit_code: Some(0),
        signal: None,
        duration_ns: 1,
        timed_out: false,
        cancelled: false,
        runner_error: None,
        launch_error: None,
        log_state: LogState::Complete,
        log_path: None,
        diagnostic: String::new(),
        cached: false,
        flaky: false,
        baseline: None,
    }
}

#[test]
fn ids_and_command_tokens_use_fixed_safe_alphabet() {
    let id = random_hex_128().expect("rng");
    assert_eq!(id.len(), 32);
    assert!(
        id.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );

    let token = command_token("cmd/with:separators");
    assert_eq!(token.len(), 64);
    assert!(
        token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert!(!result_filename(3, "cmd/with:separators", &id, &id).contains("cmd/with"));
}

#[test]
fn exact_json_parser_rejects_trailing_suffix() {
    let mut bytes = serde_json::to_vec(&result()).expect("json");
    bytes.extend_from_slice(b"\n{}");
    assert!(parse_exact_json::<CommandResultV2>(&bytes).is_err());
}

#[test]
fn private_result_file_round_trips_and_rejects_stale_destination() {
    let temp = tempfile::tempdir().expect("tempdir");
    let result = result();
    let dir = create_invocation_dir(temp.path(), &result.invocation_id, &result.runner_nonce)
        .expect("private dir");
    let parsed = write_result_file(&dir, &result).expect("write result");
    assert_eq!(parsed.command_id, result.command_id);
    assert!(write_result_file(&dir, &result).is_err());
}

#[test]
fn identity_mismatch_is_rejected_by_exact_parser_path() {
    let expected = result();
    let mut actual = expected.clone();
    actual.command_id = "other".to_string();
    assert!(verify_result_identity(&expected, &actual).is_err());
}
