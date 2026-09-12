use super::*;
use crate::legacy_core::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[test]
fn async_version_read_yields_while_file_io_is_queued() {
    use std::future::Future;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;

    let codex_home = tempdir().expect("temp codex home");
    let version_file = codex_home.path().join("version.json");
    std::fs::write(
        &version_file,
        r#"{"latest_version":"999.1.0","last_checked_at":"2026-01-02T03:04:05Z","dismissed_version":"998.0.0"}"#,
    )
    .expect("write cached release");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("test runtime");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let blocker = runtime.spawn_blocking(move || {
        started_tx
            .send(())
            .expect("signal occupied file I/O worker");
        // Dropping release_tx also releases the worker during panic unwinding,
        // so a failed assertion cannot hang runtime shutdown.
        let _ = release_rx.recv();
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("file I/O worker should start");

    runtime.block_on(async {
        let mut read = Box::pin(read_version_info_async(&version_file));
        assert!(matches!(
            read.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        drop(release_tx);
        let info = read
            .await
            .expect("read cached release after worker release");
        assert_eq!(info.latest_version, "999.1.0");
        assert_eq!(
            info.last_checked_at,
            DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .expect("expected timestamp")
                .with_timezone(&Utc)
        );
        assert_eq!(info.dismissed_version.as_deref(), Some("998.0.0"));
        blocker.await.expect("file I/O worker should finish");
    });
}

#[tokio::test]
async fn dismiss_version_creates_cache_file_when_missing() {
    let codex_home = tempdir().expect("temp codex home");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load config");
    let version_file = version_filepath(&config);

    dismiss_version(&config, "999.0.0")
        .await
        .expect("dismiss version");

    let info = read_version_info(&version_file).expect("read version info");
    assert_eq!(info.last_checked_at, DateTime::<Utc>::UNIX_EPOCH);
    assert_eq!(
        (
            info.latest_version.as_str(),
            info.dismissed_version.as_deref()
        ),
        ("999.0.0", Some("999.0.0"))
    );
}

#[tokio::test]
async fn dismiss_version_preserves_cached_release_metadata() {
    let codex_home = tempdir().expect("temp codex home");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load config");
    let version_file = version_filepath(&config);
    tokio::fs::write(
        &version_file,
        r#"{"latest_version":"999.1.0","last_checked_at":"2026-01-02T03:04:05Z","dismissed_version":"998.0.0"}"#,
    )
    .await
    .expect("write cached release");

    dismiss_version(&config, "999.0.0")
        .await
        .expect("dismiss version");

    let persisted: serde_json::Value = serde_json::from_str(
        &tokio::fs::read_to_string(&version_file)
            .await
            .expect("read persisted dismissal"),
    )
    .expect("valid cache JSON");
    assert_eq!(
        persisted,
        serde_json::json!({
            "latest_version": "999.1.0",
            "last_checked_at": "2026-01-02T03:04:05Z",
            "dismissed_version": "999.0.0",
        })
    );
}

#[tokio::test]
async fn dismiss_version_replaces_malformed_cache() {
    let codex_home = tempdir().expect("temp codex home");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load config");
    let version_file = version_filepath(&config);
    tokio::fs::write(&version_file, "{interrupted write")
        .await
        .expect("write malformed cache");

    dismiss_version(&config, "999.0.0")
        .await
        .expect("dismiss version");

    let persisted: serde_json::Value = serde_json::from_str(
        &tokio::fs::read_to_string(&version_file)
            .await
            .expect("read persisted dismissal"),
    )
    .expect("valid cache JSON");
    assert_eq!(
        persisted,
        serde_json::json!({
            "latest_version": "999.0.0",
            "last_checked_at": "1970-01-01T00:00:00Z",
            "dismissed_version": "999.0.0",
        })
    );
}
