use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use codex_code_mode_protocol::CodeModeSessionProvider;

use super::ProcessOwnedCodeModeSession;
use super::ProcessOwnedCodeModeSessionProvider;
use super::resolve_host_program;
use crate::NoopCodeModeSessionDelegate;

#[test]
fn provider_reuses_its_live_process_host() {
    let provider = ProcessOwnedCodeModeSessionProvider::default();

    let first = provider.process_host();
    let second = provider.process_host();

    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn shared_host_lease_is_released_after_the_final_session_drops() {
    let provider = ProcessOwnedCodeModeSessionProvider::default();
    let process_host = provider.process_host();
    let first = ProcessOwnedCodeModeSession::with_process_host(
        Arc::new(NoopCodeModeSessionDelegate),
        Arc::clone(&process_host),
    );
    let second = ProcessOwnedCodeModeSession::with_process_host(
        Arc::new(NoopCodeModeSessionDelegate),
        Arc::clone(&process_host),
    );

    assert_eq!(
        process_host
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_sessions,
        2
    );
    drop(first);
    assert_eq!(
        process_host
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_sessions,
        1
    );
    drop(second);
    assert_eq!(
        process_host
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_sessions,
        0
    );
}

#[tokio::test]
async fn shutdown_releases_final_host_lease() {
    let provider = ProcessOwnedCodeModeSessionProvider::default();
    let process_host = provider.process_host();
    let first = ProcessOwnedCodeModeSession::with_process_host(
        Arc::new(NoopCodeModeSessionDelegate),
        Arc::clone(&process_host),
    );
    let second = ProcessOwnedCodeModeSession::with_process_host(
        Arc::new(NoopCodeModeSessionDelegate),
        Arc::clone(&process_host),
    );

    first.shutdown().await.expect("shutdown first session");
    first
        .shutdown()
        .await
        .expect("repeated shutdown remains idempotent");
    assert_eq!(
        process_host
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_sessions,
        1
    );

    second.shutdown().await.expect("shutdown second session");
    assert_eq!(
        process_host
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_sessions,
        0
    );

    let error = match second
        .execute(codex_code_mode_protocol::ExecuteRequest {
            state_path: None,
            tool_call_id: "call-after-shutdown".to_string(),
            enabled_tools: Vec::new().into(),
            source: "text('unreachable')".to_string(),
            yield_time_ms: None,
            max_output_tokens: None,
            default_tool_timeout_ms: None,
        })
        .await
    {
        Ok(_) => panic!("closed session should remain alive and reject execution"),
        Err(error) => error,
    };
    assert_eq!(error, "code mode session is shutting down");
}

#[test]
fn host_program_resolution_preserves_override_sibling_and_fallback_precedence() {
    for (override_path, current_exe, expected) in [
        (
            Some("custom-code-mode-host".into()),
            Ok(PathBuf::from("/opt/codex/bin/codex")),
            PathBuf::from("custom-code-mode-host"),
        ),
        (
            Some("custom-code-mode-host".into()),
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "missing executable",
            )),
            PathBuf::from("custom-code-mode-host"),
        ),
        (
            None,
            Ok(PathBuf::from("/opt/codex/bin/codex")),
            PathBuf::from("/opt/codex/bin").join("codex-code-mode-host.exe"),
        ),
        (
            None,
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "missing executable",
            )),
            PathBuf::from("codex-code-mode-host.exe"),
        ),
    ] {
        assert_eq!(resolve_host_program(override_path, current_exe), expected);
    }
}

#[tokio::test]
async fn provider_reports_host_spawn_failure() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        "codex-code-mode-host-does-not-exist".into(),
    );

    let error = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .err()
        .expect("session creation should fail");

    assert!(error.contains("failed to spawn code-mode host"));
}

#[tokio::test]
async fn shutdown_before_open_does_not_spawn_the_host() {
    let session = ProcessOwnedCodeModeSession::new();

    session.shutdown().await.expect("shutdown session");
    let error = session
        .execute(codex_code_mode_protocol::ExecuteRequest {
            state_path: None,
            tool_call_id: "call-1".to_string(),
            enabled_tools: Vec::new().into(),
            source: "text('unreachable')".to_string(),
            yield_time_ms: None,
            max_output_tokens: None,
            default_tool_timeout_ms: None,
        })
        .await
        .err()
        .expect("shutdown session should reject execution");

    assert_eq!(error, "code mode session is shutting down");
}

#[tokio::test]
async fn stale_wait_pruning_preserves_newer_retained_result() {
    let key = (2, codex_code_mode_protocol::CellId::new("newer".into()));
    let release = tokio_util::sync::CancellationToken::new();
    let retained = super::RetainedWait {
        future: Box::pin(async { Err("retained result".into()) }),
        release: release.clone(),
    };
    let mut waits = std::collections::HashMap::from([(
        key.clone(), Arc::new(tokio::sync::Mutex::new(Some(retained))),
    )]);
    super::prune_pending_waits(&mut waits, 1);
    assert!(!release.is_cancelled());
    let retained = waits.get(&key).unwrap().lock().await.take().unwrap();
    assert_eq!(retained.future.await, Err("retained result".into()));
}

#[tokio::test]
async fn cancelled_queued_wait_slots_are_reclaimed_before_admission() {
    let mut waits = std::collections::HashMap::new();
    for index in 0..codex_code_mode_protocol::host::MAX_IN_FLIGHT_REQUESTS {
        let pending = Arc::new(tokio::sync::Mutex::new(None));
        waits.insert((1, codex_code_mode_protocol::CellId::new(index.to_string())), Arc::clone(&pending));
        let first = pending.lock().await;
        let mut queued = Box::pin(pending.lock());
        let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
        assert!(std::future::Future::poll(queued.as_mut(), &mut context).is_pending());
        drop(first);
        drop(queued);
        drop(pending);
    }
    super::prune_pending_waits(&mut waits, 1);
    assert!(waits.is_empty());
}

#[test]
fn stale_execute_cannot_erase_replacement_generation_state() {
    let session = ProcessOwnedCodeModeSession::new();
    session.inner.record_execution_generation(1);
    session.inner.record_execution_generation(2);
    // The older execute resumes after a replacement host has executed work.
    session.inner.record_execution_generation(1);
    assert_eq!(session.inner.executed_generation.load(std::sync::atomic::Ordering::Acquire), 2);
}
