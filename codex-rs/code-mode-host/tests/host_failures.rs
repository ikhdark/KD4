//! Host start-up failures (validation T5): sessions that queue behind one
//! wedged host must share its failure instead of paying one handshake timeout
//! each.
#![cfg(windows)]
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::NoopCodeModeSessionDelegate;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use serde_json::json;

/// A host program that accepts stdin but never answers the handshake.
fn wedged_host(directory: &tempfile::TempDir) -> std::path::PathBuf {
    let stub = directory.path().join("wedged-host.cmd");
    std::fs::write(&stub, "@echo off\r\nping -n 3600 127.0.0.1 >nul\r\n").expect("write stub");
    stub
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_share_one_wedged_host_failure() {
    let directory = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ProcessOwnedCodeModeSessionProvider::with_host_program(
        wedged_host(&directory),
    ));
    let started_at = Instant::now();
    let attempts = (0..8)
        .map(|_| {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                let result = provider
                    .create_session(Arc::new(NoopCodeModeSessionDelegate))
                    .await;
                (result.err(), started_at.elapsed())
            })
        })
        .collect::<Vec<_>>();
    let mut concurrent = Vec::new();
    for attempt in attempts {
        let (error, elapsed) = attempt.await.expect("join session attempt");
        let error = error.expect("a wedged host must not open a session");
        assert!(error.contains("timed out negotiating"), "{error}");
        concurrent.push(elapsed);
    }

    // A later caller starts a fresh attempt; nothing caches the failure.
    let retry_started_at = Instant::now();
    let retry = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await;
    let sequential_retry = retry_started_at.elapsed();
    assert!(retry.is_err(), "a wedged host must not open a session");

    concurrent.sort();
    let seconds = |duration: &Duration| (duration.as_secs_f64() * 100.0).round() / 100.0;
    println!(
        "{}",
        json!({
            "benchmark": "code_mode_wedged_host",
            "concurrent_failure_seconds": concurrent.iter().map(seconds).collect::<Vec<_>>(),
            "sequential_retry_seconds": seconds(&sequential_retry),
        })
    );
    let slowest = concurrent[concurrent.len() - 1];
    assert!(
        slowest < Duration::from_secs(25),
        "queued sessions retried the wedged host one after another: {concurrent:?}"
    );
}
