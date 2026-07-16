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

#[cfg(unix)]
#[test]
fn preexisting_symlink_temporary_is_rejected_without_following_it() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let result = result();
    let dir = create_invocation_dir(temp.path(), &result.invocation_id, &result.runner_nonce)
        .expect("private dir");
    let target = temp.path().join("target");
    fs::write(&target, b"unchanged").expect("target");
    let temporary = dir.join(format!(
        "{}.tmp",
        result_filename(
            result.command_ordinal,
            &result.command_id,
            &result.invocation_id,
            &result.runner_nonce,
        )
    ));
    symlink(&target, &temporary).expect("symlink");

    assert!(write_result_file(&dir, &result).is_err());
    assert_eq!(fs::read(&target).expect("target bytes"), b"unchanged");
}

#[test]
fn published_result_is_regular_and_identity_stable_on_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let result = result();
    let dir = create_invocation_dir(temp.path(), &result.invocation_id, &result.runner_nonce)
        .expect("private dir");
    write_result_file(&dir, &result).expect("write result");
    let path = dir.join(result_filename(
        result.command_ordinal,
        &result.command_id,
        &result.invocation_id,
        &result.runner_nonce,
    ));
    let directory = ResultDirectory::open(&dir).expect("directory handle");
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("name");
    let first = open_existing_private_file_at(&directory, name).expect("first open");
    let second = open_existing_private_file_at(&directory, name).expect("second open");
    assert_eq!(
        file_identity(&first).expect("first identity"),
        file_identity(&second).expect("second identity")
    );
    assert_eq!(
        read_result_file(&path).expect("strict read").command_id,
        result.command_id
    );
}

#[test]
fn atomic_publication_never_replaces_an_existing_destination() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source.tmp");
    let destination = temp.path().join("destination.json");
    let directory = ResultDirectory::open(temp.path()).expect("directory handle");
    let mut source_file =
        open_new_private_file_at(&directory, "source.tmp").expect("source handle");
    source_file.write_all(b"new").expect("source bytes");
    source_file.sync_all().expect("source sync");
    let mut destination_file =
        open_new_private_file_at(&directory, "destination.json").expect("destination handle");
    destination_file
        .write_all(b"existing")
        .expect("destination bytes");
    destination_file.sync_all().expect("destination sync");
    drop(destination_file);

    assert!(
        atomic_rename_no_replace_at(&directory, "source.tmp", "destination.json", &source_file,)
            .is_err()
    );
    assert_eq!(
        fs::read(&destination).expect("destination bytes"),
        b"existing"
    );
    assert_eq!(fs::read(&source).expect("source bytes"), b"new");

    fs::remove_file(&destination).expect("remove destination");
    atomic_rename_no_replace_at(&directory, "source.tmp", "destination.json", &source_file)
        .expect("publish");
    assert!(!source.exists());
    assert_eq!(fs::read(&destination).expect("published bytes"), b"new");
}
