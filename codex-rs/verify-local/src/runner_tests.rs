use super::*;
use crate::model::PlanMode;
use crate::model::RawPath;
use crate::secure_result;
use std::fs;

#[test]
fn operating_system_ids_use_fixed_lowercase_hex() {
    let first = random_hex_128().expect("rng");
    let second = random_hex_128().expect("rng");
    assert_eq!(first.len(), 32);
    assert!(
        first
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_ne!(first, second);
}

#[test]
fn diagnostic_keeps_bounded_head_and_tail() {
    let bytes = (0..(100 * 1024))
        .map(|index| if index < 50 * 1024 { b'a' } else { b'z' })
        .collect::<Vec<_>>();
    let diagnostic = bounded_diagnostic(&bytes);
    assert!(diagnostic.len() <= 64 * 1024 + 64);
    assert!(diagnostic.contains("output omitted"));
}

fn plan_for_shell(script: &str, timeout_ms: u64) -> PlanEnvelopeV2 {
    let mut plan = PlanEnvelopeV2::new(
        PlanMode::Fast,
        secure_result::random_hex_128().expect("invocation id"),
    );
    #[cfg(windows)]
    let args = vec![
        CommandArgV2::text("cmd"),
        CommandArgV2::text("/C"),
        CommandArgV2::text(script),
    ];
    #[cfg(not(windows))]
    let args = vec![
        CommandArgV2::text("sh"),
        CommandArgV2::text("-c"),
        CommandArgV2::text(script),
    ];
    plan.commands.push(CommandSpecV2 {
        id: "owner:test/command".to_string(),
        kind: "owner_test".to_string(),
        args,
        cwd: RawPath::from_utf8("."),
        timeout_ms,
        owner_packages: Vec::new(),
        hash_paths: Vec::new(),
        reason: String::new(),
    });
    plan
}

#[test]
fn execute_plan_writes_framed_log_and_strict_result_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script = "echo stdout-line && echo stderr-line 1>&2";
    let mut plan = plan_for_shell(script, 5_000);
    plan.commands[0].cwd = RawPath::from_utf8(temp.path().to_string_lossy().into_owned());
    let results = execute_plan(&plan, temp.path());
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.log_state, LogState::Complete);
    let log_path = result.log_path.as_ref().expect("log path");
    let log = fs::read_to_string(log_path).expect("log");
    assert!(log.contains("\"seq\":0"));
    assert!(log.contains("\"stream\":\"stdout\"") || log.contains("\"stream\":\"stderr\""));
    assert!(result.diagnostic.contains("bytes_base64"));

    let result_files = fs::read_dir(temp.path().join(secure_result::RESULT_ROOT))
        .expect("result root")
        .flat_map(|entry| fs::read_dir(entry.expect("entry").path()).expect("private dir"))
        .map(|entry| {
            entry
                .expect("result file")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(result_files.len(), 1);
    assert!(!result_files[0].contains("owner:test/command"));
}

#[test]
fn timeout_marks_incomplete_after_termination_without_success() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut plan = plan_for_shell(
        if cfg!(windows) {
            "ping -n 3 127.0.0.1 >NUL"
        } else {
            "sleep 2"
        },
        10,
    );
    plan.commands[0].cwd = RawPath::from_utf8(temp.path().to_string_lossy().into_owned());
    let results = execute_plan(&plan, temp.path());
    let result = &results[0];
    assert!(result.timed_out);
    assert_eq!(result.exit_code, None);
    assert_eq!(result.log_state, LogState::IncompleteAfterTermination);
}
