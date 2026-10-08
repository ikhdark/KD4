#![allow(dead_code)]
// Isolated probe of the actual event-log source; no HTTP-client dependency.
pub type ExecServerError = std::io::Error;
pub type ProcessId = String;
// Wire-only dependencies are stubbed; the measured event owner is included verbatim.
mod protocol {
    pub struct ExecParams;
    pub struct ProcessSignal;
    pub struct ReadResponse;
    pub struct WriteResponse;
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ByteChunk(pub Vec<u8>);
    impl From<Vec<u8>> for ByteChunk { fn from(value: Vec<u8>) -> Self { Self(value) } }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ExecOutputStream { Stdout, Stderr, Pty }
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ProcessOutputChunk { pub seq: u64, pub stream: ExecOutputStream, pub chunk: ByteChunk }
}
#[path = "../codex-rs/exec-server/src/process.rs"]
mod process;
use base64::Engine as _;
use std::hint::black_box;
use std::time::Instant;

fn report(label: &str, mut values: Vec<f64>) {
    values.sort_by(f64::total_cmp);
    println!("{label}: median_us={:.3} range_us={:.3}..{:.3} n={}", values[values.len()/2], values[0], values[values.len()-1], values.len());
}

fn main() {
    let log = process::ExecProcessEventLog::new(256, 1024 * 1024);
    for seq in 1..=16 {
        log.publish(process::ExecProcessEvent::Output(protocol::ProcessOutputChunk {
            seq, stream: protocol::ExecOutputStream::Stdout, chunk: vec![42; 65536].into(),
        }));
    }
    let mut snapshots = Vec::new();
    for _ in 0..9 {
        let start = Instant::now();
        for _ in 0..200 { black_box(log.subscribe()); }
        snapshots.push(start.elapsed().as_secs_f64() * 1e6 / 200.0);
    }
    report("source_event_replay_1MiB", snapshots);
    let bytes = vec![42; (64 * 1024 * 1024 - 1024 * 1024) / 4 * 3];
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let mut encode = Vec::new();
    let mut decode = Vec::new();
    for _ in 0..9 {
        let start = Instant::now();
        black_box(base64::engine::general_purpose::STANDARD.encode(black_box(&bytes)));
        encode.push(start.elapsed().as_secs_f64() * 1e6);
        let start = Instant::now();
        black_box(base64::engine::general_purpose::STANDARD.decode(black_box(&encoded)).unwrap());
        decode.push(start.elapsed().as_secs_f64() * 1e6);
    }
    report("max_file_base64_encode", encode);
    report("max_file_base64_decode", decode);
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let root = std::env::temp_dir().join(format!("codex-concurrency-probe-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let mut old = Vec::new();
        let mut batch = Vec::new();
        for round in 0..10 {
            for batched in if round % 2 == 0 { [false, true] } else { [true, false] } {
                let paths = (0..64).map(|n| root.join(n.to_string())).collect::<Vec<_>>();
                for path in &paths { std::fs::write(path, b"test").unwrap(); }
                let start = Instant::now();
                for path in paths {
                    if batched {
                        tokio::task::spawn_blocking(move || {
                            assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
                            std::fs::remove_file(path).unwrap();
                        }).await.unwrap();
                    } else {
                        assert!(tokio::fs::symlink_metadata(&path).await.unwrap().is_file());
                        tokio::fs::remove_file(path).await.unwrap();
                    }
                }
                if round != 0 {
                    let us = start.elapsed().as_secs_f64() * 1e6 / 64.0;
                    if batched { batch.push(us); } else { old.push(us); }
                }
            }
        }
        std::fs::remove_dir(root).unwrap();
        report("remove_two_hops_reproduction", old);
        report("remove_one_hop_reproduction", batch);
    });
}
