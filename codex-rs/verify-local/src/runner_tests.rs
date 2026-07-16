use super::*;
use crate::model::PlanMode;
use crate::model::PlanRequest;
use crate::model::RawPath;
use crate::model::RepositorySnapshot;
use crate::secure_result;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

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
fn planner_generated_invocation_id_executes_through_secure_runner() {
    let repository = tempfile::tempdir().expect("temporary repository");
    let changed = RawPath::from_utf8("codex-rs/verify-local/src/lib.rs");
    let snapshot = RepositorySnapshot::from_explicit_paths(repository.path(), [changed.clone()])
        .expect("explicit snapshot");
    let request = PlanRequest {
        mode: Some(PlanMode::Fast),
        changed: vec![changed],
        ..PlanRequest::default()
    };
    let mut plan = crate::planner::plan_verification(request, snapshot);
    assert_eq!(plan.invocation_id.len(), 32);
    assert!(
        plan.invocation_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    plan.verdict = None;
    plan.commands = plan_for_shell("echo planner-runner", 5_000).commands;

    let results = execute_plan(&plan, repository.path());
    let finalized = crate::finalize::finalize_plan(plan, results);
    assert_eq!(finalized.verdict, crate::model::Verdict::Verified);
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
    assert_eq!(
        result.log_state,
        LogState::Complete,
        "runner error: {:?}",
        result.runner_error
    );
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
fn mixed_32_mib_output_stays_framed_and_diagnostic_is_bounded() {
    let temp = tempfile::tempdir().expect("tempdir");
    let payload = vec![b'x'; 16 * 1024 * 1024];
    fs::write(temp.path().join("payload.bin"), &payload).expect("payload");
    let script = if cfg!(windows) {
        "type payload.bin & type payload.bin 1>&2"
    } else {
        "cat payload.bin; cat payload.bin >&2"
    };
    let mut plan = plan_for_shell(script, 30_000);
    plan.commands[0].cwd = RawPath::from_utf8(temp.path().to_string_lossy().into_owned());

    let result = execute_plan(&plan, temp.path()).remove(0);
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(
        result.log_state,
        LogState::Complete,
        "{:?}",
        result.runner_error
    );
    assert!(result.diagnostic.len() <= 64 * 1024 + 64);

    let file = fs::File::open(result.log_path.expect("log path")).expect("log");
    let mut expected_seq = 0_u64;
    let mut previous_time = 0_u64;
    let mut total_payload = 0_usize;
    let mut streams = std::collections::HashSet::new();
    for line in BufReader::new(file).split(b'\n') {
        let line = line.expect("frame line");
        if line.is_empty() {
            continue;
        }
        let frame: OwnedLogFrame = serde_json::from_slice(&line).expect("frame json");
        assert_eq!(frame.seq, expected_seq);
        assert!(frame.monotonic_ns >= previous_time);
        let decoded = BASE64_STANDARD
            .decode(frame.bytes_base64.as_bytes())
            .expect("payload base64");
        assert!(decoded.len() <= MAX_FRAME_PAYLOAD);
        total_payload += decoded.len();
        streams.insert(frame.stream);
        expected_seq += 1;
        previous_time = frame.monotonic_ns;
    }
    assert_eq!(total_payload, 32 * 1024 * 1024);
    assert_eq!(streams.len(), 2);
}

#[test]
fn cancellation_is_reported_as_fact_and_finalized_as_inconclusive() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut plan = plan_for_shell(
        if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        },
        60_000,
    );
    plan.commands[0].cwd = RawPath::from_utf8(temp.path().to_string_lossy().into_owned());
    let cancellation = Arc::new(AtomicBool::new(false));
    let trigger = Arc::clone(&cancellation);
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        trigger.store(true, Ordering::Release);
    });

    let results = execute_plan_with_cancellation(&plan, temp.path(), &cancellation);
    canceller.join().expect("canceller");
    assert!(results[0].cancelled);
    assert!(!results[0].timed_out);
    assert_eq!(results[0].log_state, LogState::IncompleteAfterTermination);
    let finalized = crate::finalize::finalize_plan(plan, results);
    assert_eq!(finalized.verdict, crate::model::Verdict::Inconclusive);
    assert!(!finalized.cache_eligible);
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
    assert_eq!(
        result.log_state,
        LogState::IncompleteAfterTermination,
        "runner error: {:?}",
        result.runner_error
    );
}

#[cfg(windows)]
#[test]
fn windows_timeout_terminates_descendants_in_the_job() {
    use std::thread;
    use std::time::Duration;

    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(
        temp.path().join("spawn-descendant.cmd"),
        "@echo off\r\nstart \"\" /b cmd.exe /d /s /c \"ping -n 3 127.0.0.1 ^>nul ^& echo escaped^>escaped.txt\"\r\nping -n 10 127.0.0.1 >nul\r\n",
    )
    .expect("script");
    let mut plan = plan_for_shell("spawn-descendant.cmd", 500);
    plan.commands[0].cwd = RawPath::from_utf8(temp.path().to_string_lossy().into_owned());

    let result = execute_plan(&plan, temp.path()).remove(0);
    assert!(result.timed_out);
    thread::sleep(Duration::from_secs(3));
    assert!(
        !temp.path().join("escaped.txt").exists(),
        "a descendant escaped the verifier Job Object"
    );
}

#[cfg(windows)]
#[test]
fn windows_process_tree_requires_suspended_verified_job_membership() {
    let source = include_str!("runner.rs");
    for required in [
        "CREATE_SUSPENDED",
        "AssignProcessToJobObject",
        "IsProcessInJob",
        "NtResumeProcess",
        "TerminateJobObject",
        "JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE",
    ] {
        assert!(source.contains(required), "missing {required}");
    }
    assert!(!source.contains("JOB_OBJECT_LIMIT_BREAKAWAY_OK"));
    assert!(!source.contains("JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK"));
}
