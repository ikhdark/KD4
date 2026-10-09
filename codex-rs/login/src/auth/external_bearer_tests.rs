use super::super::manager::ExternalAuthRefreshReason;
use super::*;
use std::future::Future;
use std::time::Duration;

struct Provider {
    home: tempfile::TempDir,
    source: Arc<BearerTokenRefresher>,
}

impl Provider {
    fn new() -> Self {
        Self::with_timing(60_000, 10_000)
    }

    fn with_timing(refresh_interval_ms: u64, timeout_ms: u64) -> Self {
        let home = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let (command, args, name, script) = (
            "cmd.exe",
            vec!["/d", "/s", "/c", "provider.cmd"],
            "provider.cmd",
            "@echo off\r\nif exist fail exit /b 1\r\nif exist started goto second\r\necho started>started\r\n:wait\r\nif not exist release goto wait\r\necho first-token\r\nexit /b 0\r\n:second\r\necho started>second\r\necho second-token\r\n",
        );
        #[cfg(unix)]
        let (command, args, name, script) = (
            "sh",
            vec!["provider.sh"],
            "provider.sh",
            "[ -f fail ] && exit 1\nif [ -f started ]; then\n touch second\n echo second-token\nelse\n touch started\n while [ ! -f release ]; do sleep 0.01; done\n echo first-token\nfi\n",
        );
        std::fs::write(home.path().join(name), script).unwrap();
        let config = serde_json::from_value(serde_json::json!({
            "command": command, "args": args, "cwd": home.path(),
            "timeout_ms": timeout_ms, "refresh_interval_ms": refresh_interval_ms,
        }))
        .unwrap();
        Self {
            home,
            source: Arc::new(BearerTokenRefresher::new(config)),
        }
    }

    fn release(&self) {
        std::fs::write(self.home.path().join("release"), "").unwrap();
    }

    async fn started(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.home.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
}

fn refresh_context() -> ExternalAuthRefreshContext {
    ExternalAuthRefreshContext {
        reason: ExternalAuthRefreshReason::Unauthorized,
        previous_account_id: None,
    }
}

#[tokio::test]
async fn overlapping_ttl_and_forced_refresh_share_the_provider_command() {
    let provider = Provider::new();
    let resolving = tokio::spawn({
        let source = Arc::clone(&provider.source);
        async move { source.resolve().await }
    });
    provider.started().await;
    let mut refreshing = Box::pin(provider.source.refresh(refresh_context()));
    std::future::poll_fn(|cx| {
        assert!(refreshing.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    provider.release();
    let resolved = resolving.await.unwrap().unwrap();
    let refreshed = refreshing.await.unwrap();
    eprintln!(
        "overlapping provider refresh: duplicate_command={}",
        provider.home.path().join("second").exists()
    );
    assert_eq!(resolved.api_key(), Some("first-token"));
    assert_eq!(refreshed.api_key(), Some("first-token"));
    assert!(!provider.home.path().join("second").exists());
    // A later rejection is not the same authorization decision: it must refresh.
    assert_eq!(
        provider
            .source
            .refresh(refresh_context())
            .await
            .unwrap()
            .api_key(),
        Some("second-token")
    );
}

#[tokio::test]
async fn forced_refresh_blocks_same_provider_not_independent_providers_and_cancels_cleanly() {
    let provider = Provider::new();
    *provider.source.state.cached_token.lock().await = Some(CachedExternalBearerToken {
        access_token: "rejected-token".into(),
        fetched_at: Instant::now(),
    });
    let refreshing = tokio::spawn({
        let source = Arc::clone(&provider.source);
        async move { source.refresh(refresh_context()).await }
    });
    provider.started().await;
    let other = Provider::new();
    other.release();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), other.source.resolve())
            .await
            .unwrap()
            .unwrap()
            .api_key(),
        Some("first-token")
    );
    let mut resolving = Box::pin(provider.source.resolve());
    std::future::poll_fn(|cx| {
        assert!(
            resolving.as_mut().poll(cx).is_pending(),
            "must not hand out credentials already rejected by the authority"
        );
        std::task::Poll::Ready(())
    })
    .await;
    refreshing.abort();
    assert!(refreshing.await.unwrap_err().is_cancelled());
    provider.release();
    assert_eq!(resolving.await.unwrap().api_key(), Some("second-token"));
}

#[tokio::test]
async fn failed_forced_refresh_does_not_reuse_rejected_credentials() {
    let provider = Provider::new();
    *provider.source.state.cached_token.lock().await = Some(CachedExternalBearerToken {
        access_token: "rejected-token".into(),
        fetched_at: Instant::now(),
    });
    std::fs::write(provider.home.path().join("fail"), "").unwrap();
    assert!(provider.source.refresh(refresh_context()).await.is_err());
    assert!(provider.source.resolve().await.is_err());
    std::fs::remove_file(provider.home.path().join("fail")).unwrap();
    provider.release();
    assert_eq!(
        provider.source.resolve().await.unwrap().api_key(),
        Some("first-token")
    );
}

#[tokio::test]
async fn concurrent_forced_refreshes_share_only_the_overlapping_acquisition() {
    let provider = Provider::with_timing(0, 10_000);
    let first = tokio::spawn({
        let source = Arc::clone(&provider.source);
        async move { source.refresh(refresh_context()).await }
    });
    provider.started().await;
    let mut second = Box::pin(provider.source.refresh(refresh_context()));
    std::future::poll_fn(|cx| {
        assert!(second.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    provider.release();
    assert_eq!(first.await.unwrap().unwrap().api_key(), Some("first-token"));
    assert_eq!(second.await.unwrap().api_key(), Some("first-token"));
    assert!(!provider.home.path().join("second").exists());
    assert_eq!(
        provider
            .source
            .refresh(refresh_context())
            .await
            .unwrap()
            .api_key(),
        Some("second-token")
    );
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "hold admission to expire the acquired token before the waiter samples it"
)]
async fn refresh_waiter_never_reuses_an_expired_acquisition() {
    let provider = Provider::with_timing(1, 10_000);
    provider.release();
    let mut guard = provider.source.state.cached_token.lock().await;
    let mut refreshing = Box::pin(provider.source.refresh(refresh_context()));
    std::future::poll_fn(|cx| {
        assert!(refreshing.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    *guard = Some(CachedExternalBearerToken {
        access_token: "expired-overlapping-token".into(),
        fetched_at: Instant::now(),
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(guard);
    assert_eq!(refreshing.await.unwrap().api_key(), Some("first-token"));
}

#[tokio::test]
async fn timed_out_refresh_releases_provider_admission() {
    let provider = Provider::with_timing(60_000, 1_000);
    let error = provider
        .source
        .refresh(refresh_context())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    provider.release();
    assert!(provider.source.resolve().await.is_ok());
}
