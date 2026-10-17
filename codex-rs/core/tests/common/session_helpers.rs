//! Config and event helpers shared by `core_test_support` and codex-core's
//! unit tests (`codex_core::test_harness`).

use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_core::CodexThread;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use std::path::Path;
use tempfile::TempDir;

/// Returns a default `Config` whose on-disk state is confined to the provided
/// temporary directory. Using a per-test directory keeps tests hermetic and
/// avoids clobbering a developer’s real `~/.codex`.
pub async fn load_default_config_for_test(codex_home: &TempDir) -> Config {
    load_default_config_for_test_with_cloud_config_bundle(
        codex_home,
        CloudConfigBundleLoader::default(),
    )
    .await
}

/// Returns a default `Config` with test-provided cloud bundle requirements applied.
/// during config construction.
pub async fn load_default_config_for_test_with_cloud_config_bundle(
    codex_home: &TempDir,
    cloud_config_bundle: CloudConfigBundleLoader,
) -> Config {
    let mut config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(default_test_overrides(codex_home.path()))
        .cloud_config_bundle(cloud_config_bundle)
        .build()
        .await
        .expect("defaults for test should always succeed");
    // Do not let a developer-level CODEX_SQLITE_HOME override make otherwise
    // hermetic integration tests share one state database.
    config.sqlite_home = codex_home.path().join("sqlite");
    config
}

fn default_test_overrides(codex_home: &Path) -> ConfigOverrides {
    ConfigOverrides {
        cwd: Some(codex_home.to_path_buf()),
        ..ConfigOverrides::default()
    }
}

pub async fn wait_for_event<F>(
    codex: &CodexThread,
    predicate: F,
) -> codex_protocol::protocol::EventMsg
where
    F: FnMut(&codex_protocol::protocol::EventMsg) -> bool,
{
    wait_for_event_with_timeout(codex, predicate, INTEGRATION_EVENT_TIMEOUT).await
}

pub async fn wait_for_event_match<T, F>(codex: &CodexThread, matcher: F) -> T
where
    F: Fn(&codex_protocol::protocol::EventMsg) -> Option<T>,
{
    wait_for_event_match_with_timeout(codex, matcher, INTEGRATION_EVENT_TIMEOUT).await
}

const INTEGRATION_EVENT_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);

pub async fn wait_for_event_match_with_timeout<T, F>(
    codex: &CodexThread,
    matcher: F,
    wait_time: tokio::time::Duration,
) -> T
where
    F: Fn(&codex_protocol::protocol::EventMsg) -> Option<T>,
{
    let ev = wait_for_event_with_timeout(codex, |ev| matcher(ev).is_some(), wait_time).await;
    matcher(&ev).expect("EventMsg should match matcher predicate")
}

pub async fn wait_for_event_with_timeout<F>(
    codex: &CodexThread,
    mut predicate: F,
    wait_time: tokio::time::Duration,
) -> codex_protocol::protocol::EventMsg
where
    F: FnMut(&codex_protocol::protocol::EventMsg) -> bool,
{
    wait_for_event_envelope_with_timeout(codex, |ev| predicate(&ev.msg), wait_time)
        .await
        .msg
}

/// Preserve operation IDs for scenarios with multiple submissions or objects.
pub async fn wait_for_event_envelope_with_timeout<F>(
    codex: &CodexThread,
    predicate: F,
    wait_time: tokio::time::Duration,
) -> codex_protocol::protocol::Event
where
    F: FnMut(&codex_protocol::protocol::Event) -> bool,
{
    event_before_deadline(
        wait_time,
        || async { codex.next_event().await.expect("stream ended unexpectedly") },
        predicate,
    )
    .await
    .expect("timeout waiting for event")
}

pub(crate) async fn event_before_deadline<T, N, E, P>(
    wait_time: tokio::time::Duration,
    mut next_event: N,
    mut predicate: P,
) -> Result<T, tokio::time::error::Elapsed>
where
    N: FnMut() -> E,
    E: std::future::Future<Output = T>,
    P: FnMut(&T) -> bool,
{
    let deadline = tokio::time::Instant::now() + wait_time;
    tokio::time::timeout_at(deadline, async {
        loop {
            let event = next_event().await;
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
}
