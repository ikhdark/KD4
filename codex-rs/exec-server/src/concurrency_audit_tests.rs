//! Opt-in, narrow measurements; no timing assertions on shared build hosts.
use std::hint::black_box;
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::ExecProcessEvent;
use crate::ExecutorFileSystem;
use crate::LocalFileSystem;
use crate::RemoveOptions;
use crate::process::ExecProcessEventLog;
use crate::protocol::ExecOutputStream;
use crate::protocol::ProcessOutputChunk;
use codex_utils_path_uri::PathUri;

fn report(label: &str, mut samples: Vec<f64>) {
    samples.sort_by(f64::total_cmp);
    eprintln!(
        "{label}: median_us={:.3} min_us={:.3} max_us={:.3} samples={}",
        samples[samples.len() / 2],
        samples[0],
        samples[samples.len() - 1],
        samples.len()
    );
}

#[test]
#[ignore = "manual concurrency microbenchmark"]
fn concurrency_bench_event_replay_and_codec() {
    let log = ExecProcessEventLog::new(256, 1024 * 1024);
    for seq in 1..=16 {
        log.publish(ExecProcessEvent::Output(ProcessOutputChunk {
            seq,
            stream: ExecOutputStream::Stdout,
            chunk: vec![42; 65536].into(),
        }));
    }
    let mut snapshots = Vec::new();
    for _ in 0..9 {
        let start = Instant::now();
        for _ in 0..200 {
            black_box(log.subscribe());
        }
        snapshots.push(start.elapsed().as_secs_f64() * 1e6 / 200.0);
    }
    report("event_replay_1MiB_snapshot", snapshots);
    let bytes = vec![42; crate::connection::MAX_FILE_PAYLOAD_BYTES];
    let encoded = STANDARD.encode(&bytes);
    let mut encode = Vec::new();
    let mut decode = Vec::new();
    for _ in 0..9 {
        let start = Instant::now();
        black_box(STANDARD.encode(black_box(&bytes)));
        encode.push(start.elapsed().as_secs_f64() * 1e6);
        let start = Instant::now();
        black_box(STANDARD.decode(black_box(&encoded)).unwrap());
        decode.push(start.elapsed().as_secs_f64() * 1e6);
    }
    report("base64_max_file_encode", encode);
    report("base64_max_file_decode", decode);
}

#[tokio::test]
#[ignore = "manual concurrency microbenchmark"]
async fn concurrency_bench_remove_handoffs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fs = LocalFileSystem::unsandboxed();
    let mut current = Vec::new();
    let mut batched = Vec::new();
    for round in 0..10 {
        // Alternate execution order to reduce cache/order bias.
        for batch in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let paths = (0..64)
                .map(|n| dir.path().join(format!("file-{n}")))
                .collect::<Vec<_>>();
            for path in &paths {
                std::fs::write(path, b"payload")?;
            }
            let start = Instant::now();
            for path in paths {
                if batch {
                    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                        assert!(std::fs::symlink_metadata(&path)?.is_file());
                        std::fs::remove_file(path)
                    })
                    .await??;
                } else {
                    fs.remove(
                        &PathUri::from_host_native_path(path)?,
                        RemoveOptions {
                            recursive: false,
                            force: false,
                        },
                        None,
                    )
                    .await?;
                }
            }
            if round != 0 {
                let elapsed = start.elapsed().as_secs_f64() * 1e6 / 64.0;
                if batch {
                    batched.push(elapsed);
                } else {
                    current.push(elapsed);
                }
            }
        }
    }
    report("production_remove_per_file", current);
    report("one_worker_remove_per_file", batched);
    Ok(())
}

#[tokio::test]
async fn removal_preserves_force_recursive_and_directory_link_behavior() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fs = LocalFileSystem::unsandboxed();
    let path = dir.path().join("target");
    let uri = PathUri::from_host_native_path(&path)?;
    let options = RemoveOptions {
        recursive: false,
        force: false,
    };
    assert_eq!(
        fs.remove(&uri, options, None).await.unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    fs.remove(
        &uri,
        RemoveOptions {
            force: true,
            ..options
        },
        None,
    )
    .await?;
    std::fs::create_dir(&path)?;
    std::fs::write(path.join("child"), b"keep")?;
    assert!(fs.remove(&uri, options, None).await.is_err());
    let link = dir.path().join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&path, &link)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&path, &link)?;
    fs.remove(
        &PathUri::from_host_native_path(&link)?,
        RemoveOptions {
            recursive: true,
            force: false,
        },
        None,
    )
    .await?;
    assert_eq!(std::fs::read(path.join("child"))?, b"keep");
    fs.remove(
        &uri,
        RemoveOptions {
            recursive: true,
            force: false,
        },
        None,
    )
    .await?;
    assert!(!path.exists());
    std::fs::write(&path, b"file")?;
    fs.remove(&uri, options, None).await?;
    assert!(!path.exists());
    Ok(())
}

#[test]
fn cancelled_queued_removal_leaves_file_intact() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()?;
    runtime.block_on(async {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("keep");
        std::fs::write(&path, b"keep")?;
        let uri = PathUri::from_host_native_path(&path)?;
        let fs = LocalFileSystem::unsandboxed();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(5));
        });
        started_rx.await?;
        let mut removal = Box::pin(fs.remove(
            &uri,
            RemoveOptions {
                recursive: false,
                force: false,
            },
            None,
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(removal.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(removal);
        release_tx.send(())?;
        worker.await?;
        tokio::task::spawn_blocking(|| ()).await?;
        assert_eq!(std::fs::read(path)?, b"keep");
        Ok(())
    })
}
