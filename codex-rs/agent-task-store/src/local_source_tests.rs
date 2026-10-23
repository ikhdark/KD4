use super::*;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

struct ActiveRead(Arc<AtomicUsize>);

impl Drop for ActiveRead {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn source_collection_refills_behind_a_stalled_first_path_and_caps_work() {
    let paths = (0..65).map(|i| format!("p{i:02}")).collect();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = AtomicUsize::new(0);
    let started = AtomicUsize::new(0);
    let ninth = tokio::sync::Notify::new();
    let hashes = tokio::time::timeout(
        Duration::from_secs(2),
        collect_current_source_hashes(&paths, |path| {
            let active = Arc::clone(&active);
            let (peak, started, ninth) = (&peak, &started, &ninth);
            async move {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                let _guard = ActiveRead(Arc::clone(&active));
                peak.fetch_max(count, Ordering::SeqCst);
                started.fetch_add(1, Ordering::SeqCst);
                if path == "p00" {
                    ninth.notified().await;
                } else if path == "p08" {
                    ninth.notify_one();
                }
                tokio::task::yield_now().await;
                Some(path)
            }
        }),
    )
    .await
    .expect("the ninth path must start while the first path is blocked");
    assert_eq!(started.load(Ordering::SeqCst), 64);
    assert!(peak.load(Ordering::SeqCst) <= 8);
    assert!(peak.load(Ordering::SeqCst) > 1);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(hashes.len(), 64);
    assert_eq!(hashes.get("p00").map(String::as_str), Some("p00"));
    assert!(!hashes.contains_key("p64"));
}

#[tokio::test]
async fn source_collection_timeout_discards_partial_proof_and_owned_futures() {
    let paths = BTreeSet::from(["ready".to_string(), "stalled".to_string()]);
    let active = Arc::new(AtomicUsize::new(0));
    let mut published = HashMap::new();
    let observed = tokio::time::timeout(
        Duration::from_millis(20),
        collect_current_source_hashes(&paths, |path| {
            let active = Arc::clone(&active);
            async move {
                active.fetch_add(1, Ordering::SeqCst);
                let _guard = ActiveRead(Arc::clone(&active));
                if path == "stalled" {
                    std::future::pending::<()>().await;
                }
                Some(path)
            }
        }),
    )
    .await;
    assert!(observed.is_err());
    if let Ok(hashes) = observed {
        published = hashes;
    }
    assert!(published.is_empty());
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn source_hashes_require_normalized_regular_bounded_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    std::fs::write(root.join("source.rs"), b"abc").unwrap();
    assert_eq!(
        read_current_source_hash(&root, "source.rs")
            .await
            .as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
    );
    std::fs::create_dir(root.join("directory")).unwrap();
    let oversized = std::fs::File::create(root.join("oversized")).unwrap();
    oversized.set_len(8 * 1024 * 1024 + 1).unwrap();
    for path in [
        "missing",
        "directory",
        "oversized",
        "../outside",
        "./source.rs",
    ] {
        assert!(
            read_current_source_hash(&root, path).await.is_none(),
            "{path}"
        );
    }
    let paths = BTreeSet::from([
        "source.rs".to_string(),
        "source.rs".to_string(),
        "missing".to_string(),
    ]);
    let root = &root;
    let hashes = collect_current_source_hashes(&paths, |path| async move {
        read_current_source_hash(root, &path).await
    })
    .await;
    assert_eq!(hashes.len(), 1);
    assert!(hashes.contains_key("source.rs"));
}
