use super::append_output_loss_markers;
use super::lagged_output_marker;
use super::omitted_output_marker;
use super::record_known_delta_from_transcript;
use super::resolve_aggregated_output;
use super::split_valid_utf8_prefix_with_max;

use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::tools::known_delta_store;
use crate::tools::known_delta_store::KnownDeltaExecutionObservation;
use crate::tools::known_delta_store::PreparedKnownDelta;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;

fn run_git(cwd: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "KD4 Test")
        .env("GIT_AUTHOR_EMAIL", "kd4@example.invalid")
        .env("GIT_COMMITTER_NAME", "KD4 Test")
        .env("GIT_COMMITTER_EMAIL", "kd4@example.invalid")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_string()
}

async fn published_candidate(
    root: &std::path::Path,
    name: &str,
    force_fresh: bool,
) -> (
    std::path::PathBuf,
    std::path::PathBuf,
    String,
    PreparedKnownDelta,
) {
    let repo = root.join(format!("repo-{name}"));
    let home = root.join(format!("home-{name}"));
    std::fs::create_dir_all(&repo).expect("create git repo");
    run_git(&repo, &["init"]);
    std::fs::write(repo.join("read.txt"), "immutable\n").expect("write immutable fixture");
    run_git(&repo, &["add", "read.txt"]);
    run_git(&repo, &["commit", "-m", "initial"]);
    let blob = run_git(&repo, &["rev-parse", "HEAD:read.txt"]);
    let args = ["show".to_string(), blob.clone()];
    let first = known_delta_store::prepare_immutable_git_show(
        &home,
        "test-thread",
        &repo,
        "git",
        &args,
        None,
        false,
    )
    .await
    .expect("immutable git show is eligible");
    known_delta_store::record_execution(
        &home,
        &first,
        KnownDeltaExecutionObservation::CompleteSuccess {
            output: b"immutable\n",
            executor_cost: Duration::from_secs(1),
        },
    )
    .await;
    let prepared = known_delta_store::prepare_immutable_git_show(
        &home,
        "test-thread",
        &repo,
        "git",
        &args,
        None,
        force_fresh,
    )
    .await
    .expect("published immutable git show remains eligible");
    (repo, home, blob, prepared)
}

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

#[tokio::test]
async fn known_delta_background_completion_promotes_only_exact_transcripts() {
    let root = tempfile::tempdir().expect("test root");
    let (repo, home, blob, prepared) = published_candidate(root.path(), "exact", false).await;
    assert!(prepared.has_candidate());
    assert!(!prepared.is_hit());
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::default()));
    transcript.lock().await.push_chunk(b"immutable\n".to_vec());

    record_known_delta_from_transcript(&home, &prepared, &transcript, true, Duration::from_secs(1))
        .await;

    let promoted = known_delta_store::prepare_immutable_git_show(
        &home,
        "next-thread",
        &repo,
        "git",
        &["show".to_string(), blob],
        None,
        false,
    )
    .await
    .expect("exact background evidence remains eligible");
    assert!(promoted.is_hit());
}

#[tokio::test]
async fn known_delta_background_completion_skips_lossy_output_and_quarantines_fresh_failure() {
    let root = tempfile::tempdir().expect("test root");

    let (omitted_repo, omitted_home, omitted_blob, omitted_prepared) =
        published_candidate(root.path(), "omitted", false).await;
    let omitted = Arc::new(Mutex::new(HeadTailBuffer::new(4)));
    omitted.lock().await.push_chunk(b"immutable\n".to_vec());
    record_known_delta_from_transcript(
        &omitted_home,
        &omitted_prepared,
        &omitted,
        true,
        Duration::from_secs(1),
    )
    .await;
    let after_omission = known_delta_store::prepare_immutable_git_show(
        &omitted_home,
        "next-thread",
        &omitted_repo,
        "git",
        &["show".to_string(), omitted_blob],
        None,
        false,
    )
    .await
    .expect("omitted evidence remains eligible as a miss");
    assert!(after_omission.has_candidate());
    assert!(!after_omission.is_hit());

    let (lagged_repo, lagged_home, lagged_blob, lagged_prepared) =
        published_candidate(root.path(), "lagged", false).await;
    let lagged = Arc::new(Mutex::new(HeadTailBuffer::default()));
    {
        let mut transcript = lagged.lock().await;
        transcript.push_chunk(b"immutable\n".to_vec());
        transcript.record_lagged_chunks(1);
    }
    record_known_delta_from_transcript(
        &lagged_home,
        &lagged_prepared,
        &lagged,
        true,
        Duration::from_secs(1),
    )
    .await;
    let after_lag = known_delta_store::prepare_immutable_git_show(
        &lagged_home,
        "next-thread",
        &lagged_repo,
        "git",
        &["show".to_string(), lagged_blob],
        None,
        false,
    )
    .await
    .expect("lagged evidence remains eligible as a miss");
    assert!(after_lag.has_candidate());
    assert!(!after_lag.is_hit());

    let (fresh_repo, fresh_home, fresh_blob, fresh_prepared) =
        published_candidate(root.path(), "force-fresh", true).await;
    assert!(fresh_prepared.has_candidate());
    let exact_failure = Arc::new(Mutex::new(HeadTailBuffer::default()));
    exact_failure
        .lock()
        .await
        .push_chunk(b"fatal: object unavailable\n".to_vec());
    record_known_delta_from_transcript(
        &fresh_home,
        &fresh_prepared,
        &exact_failure,
        false,
        Duration::from_secs(1),
    )
    .await;
    let quarantined = known_delta_store::prepare_immutable_git_show(
        &fresh_home,
        "next-thread",
        &fresh_repo,
        "git",
        &["show".to_string(), fresh_blob],
        None,
        false,
    )
    .await
    .expect("quarantined identity remains structurally eligible");
    assert!(!quarantined.has_candidate());
    assert!(!quarantined.is_hit());
}
