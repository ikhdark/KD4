//! Regression tests and opt-in benchmarks using isolated synthetic rollouts.
use codex_protocol::ThreadId;
use codex_rollout::*;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tempfile::TempDir;

fn config(home: &Path) -> RolloutConfig {
    RolloutConfig {
        codex_home: home.into(),
        sqlite_home: home.into(),
        cwd: home.into(),
        model_provider_id: "test-provider".into(),
    }
}

fn fixture(
    home: &Path,
    count: usize,
    lines: usize,
    width: usize,
    compressed: bool,
) -> anyhow::Result<Vec<PathBuf>> {
    let root = home.join("sessions/2026/09/01");
    std::fs::create_dir_all(&root)?;
    let corpus = concat!(
        include_str!("../src/recorder.rs"),
        include_str!("../src/list.rs"),
        include_str!("../src/search.rs")
    );
    let mut paths = Vec::new();
    for index in 0..count {
        let id = uuid::Uuid::from_u128(index as u128 + 1);
        let path = root.join(format!("rollout-2026-09-01T12-00-00-{id}.jsonl"));
        let mut bytes = Vec::new();
        writeln!(
            bytes,
            "{}",
            json!({"timestamp":"2026-09-01T12:00:00Z", "type":"session_meta", "payload":{"session_id":id,"id":id,"timestamp":"2026-09-01T12:00:00Z","cwd":home,"originator":"audit","cli_version":"audit","source":"cli","model_provider":"test-provider"}})
        )?;
        writeln!(
            bytes,
            "{}",
            json!({"timestamp":"2026-09-01T12:00:00Z","type":"event_msg","payload":{"type":"user_message","message":format!("task {index}"),"kind":"plain"}})
        )?;
        for line in 0..lines {
            let offset = (index * 4099 + line * 997) % (corpus.len() - width);
            let source = String::from_utf8_lossy(&corpus.as_bytes()[offset..offset + width]);
            writeln!(
                bytes,
                "{}",
                json!({"timestamp":"2026-09-01T12:00:00Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":format!("record {line} {source}")}]}})
            )?;
        }
        if compressed {
            std::fs::write(
                path.with_extension("jsonl.zst"),
                zstd::stream::encode_all(bytes.as_slice(), 3)?,
            )?;
        } else {
            std::fs::write(&path, bytes)?;
        }
        paths.push(path);
    }
    Ok(paths)
}

async fn listing(
    home: &Path,
    key: ThreadSortKey,
    direction: SortDirection,
    search: Option<&str>,
) -> std::io::Result<ThreadsPage> {
    RolloutRecorder::list_threads(
        None,
        &config(home),
        20,
        None,
        key,
        direction,
        &[],
        None,
        None,
        "test-provider",
        search,
    )
    .await
}

#[tokio::test]
async fn ascending_search_probe() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    fixture(home.path(), 2, 1, 100, false)?;
    for index in 1..=2 {
        append_thread_name(
            home.path(),
            ThreadId::from_string(&uuid::Uuid::from_u128(index).to_string())?,
            "task",
        )
        .await?;
    }
    let start = Instant::now();
    let result = listing(
        home.path(),
        ThreadSortKey::CreatedAt,
        SortDirection::Asc,
        Some("task"),
    )
    .await?;
    println!(
        "AUDIT ascending_search items={} elapsed_ms={:.3}",
        result.items.len(),
        start.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(result.items.len(), 2);
    assert_eq!(
        listing(
            home.path(),
            ThreadSortKey::CreatedAt,
            SortDirection::Asc,
            None
        )
        .await?
        .items
        .len(),
        2
    );
    assert_eq!(
        listing(
            home.path(),
            ThreadSortKey::CreatedAt,
            SortDirection::Desc,
            Some("task")
        )
        .await?
        .items
        .len(),
        2
    );
    Ok(())
}

#[tokio::test]
#[ignore]
async fn unchanged_listing_probe() -> anyhow::Result<()> {
    for count in [24, 96] {
        let home = TempDir::new()?;
        let paths = fixture(home.path(), count, 512, 8192, false)?;
        let bytes: u64 = paths
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().len())
            .sum();
        let mut previous = Vec::new();
        for run in 0..3 {
            let start = Instant::now();
            let page = listing(
                home.path(),
                ThreadSortKey::RecencyAt,
                SortDirection::Desc,
                None,
            )
            .await?;
            let ids: Vec<_> = page.items.iter().map(|i| i.thread_id).collect();
            assert_eq!(ids.len(), 20);
            if run > 0 {
                assert_eq!(ids, previous);
            }
            previous = ids;
            println!(
                "AUDIT unchanged_listing count={count} run={run} fixture_bytes={bytes} elapsed_ms={:.3}",
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore]
async fn selective_title_probe() -> anyhow::Result<()> {
    for count in [300, 1000] {
        let home = TempDir::new()?;
        fixture(home.path(), count, 64, 2048, false)?;
        for search in [None, Some("title-that-does-not-exist")] {
            let start = Instant::now();
            let page = listing(
                home.path(),
                ThreadSortKey::RecencyAt,
                SortDirection::Desc,
                search,
            )
            .await?;
            assert_eq!(page.items.len(), if search.is_some() { 0 } else { 20 });
            println!(
                "AUDIT selective_title count={count} search={search:?} elapsed_ms={:.3}",
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore]
async fn resume_probe() -> anyhow::Result<()> {
    for lines in [4096, 16384] {
        let home = TempDir::new()?;
        let path = fixture(home.path(), 1, lines, 16384, false)?.remove(0);
        let bytes = std::fs::metadata(&path)?.len();
        for run in 0..3 {
            let start = Instant::now();
            let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await?;
            let read_ms = start.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(errors, 0);
            assert_eq!(items.len(), lines + 2);
            let start = Instant::now();
            let recorder = RolloutRecorder::new_with_repository_context(
                &config(home.path()),
                RolloutRecorderParams::resume(path.clone()),
                Some(None),
            )
            .await?;
            let reopen_ms = start.elapsed().as_secs_f64() * 1000.0;
            recorder.shutdown().await?;
            println!(
                "AUDIT resume lines={lines} fixture_bytes={bytes} run={run} history_ms={read_ms:.3} reopen_ms={reopen_ms:.3}"
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn locked_backfill_probe() -> anyhow::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let home = TempDir::new()?;
    let path = fixture(home.path(), 1, 2, 100, false)?.remove(0);
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)?;
    let start = Instant::now();
    let result = state_integration::try_init(&config(home.path())).await;
    let first_ms = start.elapsed().as_secs_f64() * 1000.0;
    let error = result
        .err()
        .expect("locked rollout must leave backfill pending");
    assert!(error.to_string().contains("backfill remains incomplete"));
    let db = codex_state::StateRuntime::init(home.path().into(), "test-provider".into()).await?;
    let id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")?;
    let missing_locked = db.get_thread(id).await?.is_none();
    let state = db.get_backfill_state().await?;
    db.close().await;
    drop(held);
    let start = Instant::now();
    let db2 = state_integration::try_init(&config(home.path())).await?;
    let missing_unlocked = db2.get_thread(id).await?.is_none();
    println!(
        "AUDIT locked_backfill first_ms={first_ms:.3} second_ms={:.3} state={state:?} missing_locked={missing_locked} missing_unlocked={missing_unlocked}",
        start.elapsed().as_secs_f64() * 1000.0
    );
    assert!(missing_locked);
    assert_eq!(state.status, codex_state::BackfillStatus::Pending);
    assert_eq!(state.last_watermark, None);
    assert!(!missing_unlocked);
    assert_eq!(RolloutRecorder::load_rollout_items(&path).await?.0.len(), 4);
    Ok(())
}

#[tokio::test]
#[ignore]
async fn compressed_search_probe() -> anyhow::Result<()> {
    let rg = PathBuf::from(std::env::var("AUDIT_RG")?);
    for compressed in [false, true] {
        let home = TempDir::new()?;
        fixture(home.path(), 32, 256, 8192, compressed)?;
        for run in 0..2 {
            let start = Instant::now();
            let matches =
                search_rollout_matches(&rg, home.path(), false, "needle-not-present").await?;
            assert!(matches.is_empty());
            println!(
                "AUDIT compressed_search compressed={compressed} run={run} elapsed_ms={:.3}",
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn title_search_preserves_pagination_and_latest_names() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    fixture(home.path(), 30, 1, 100, false)?;
    for index in 1..=30 {
        append_thread_name(
            home.path(),
            ThreadId::from_string(&uuid::Uuid::from_u128(index).to_string())?,
            if index % 2 == 1 {
                "matching title"
            } else {
                "other"
            },
        )
        .await?;
    }
    append_thread_name(
        home.path(),
        ThreadId::from_string(&uuid::Uuid::from_u128(1).to_string())?,
        "renamed",
    )
    .await?;
    for direction in [SortDirection::Asc, SortDirection::Desc] {
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let page = RolloutRecorder::list_threads(
                None,
                &config(home.path()),
                3,
                cursor.as_ref(),
                ThreadSortKey::CreatedAt,
                direction,
                &[],
                None,
                None,
                "test-provider",
                Some("matching"),
            )
            .await?;
            assert!(page.items.len() <= 3);
            ids.extend(page.items.into_iter().map(|item| item.thread_id.unwrap()));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
            assert!(ids.len() < 30, "pagination must make progress");
        }
        let mut expected = (3..=29)
            .step_by(2)
            .map(|index| ThreadId::from_string(&uuid::Uuid::from_u128(index).to_string()).unwrap())
            .collect::<Vec<_>>();
        if direction == SortDirection::Desc {
            expected.reverse();
        }
        assert_eq!(ids, expected);
    }
    let empty = listing(
        home.path(),
        ThreadSortKey::RecencyAt,
        SortDirection::Desc,
        Some("absent"),
    )
    .await?;
    assert!(empty.items.is_empty());
    assert_eq!(empty.num_scanned_files, 0);
    Ok(())
}
