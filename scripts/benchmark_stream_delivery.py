#!/usr/bin/env python3
"""Benchmark actual TUI streaming primitives without building the application.

Compiles immutable copies of the owning Rust modules with the checkout's rustc.
Tracing is disabled and pretty_assertions uses std assertions in this isolated
harness. Renderer-dependent table_detect tests are excluded; chunking and
holdback tests run unchanged. This measures CPU and virtual boundaries, not
provider tokens, network latency, markdown rendering, or terminal paint.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile


HARNESS = r'''
#![allow(unused)]
extern crate self as tracing;
extern crate self as pretty_assertions;
pub use std::assert_eq;
#[macro_export]
macro_rules! trace { ($($tokens:tt)*) => {}; }
mod table_detect;
mod chunking;
mod table_holdback;
#[cfg(not(test))]
mod markdown_stream;

#[cfg(not(test))]
fn main() {
    use chunking::{AdaptiveChunkingPolicy, DrainPlan, QueueSnapshot};
    use markdown_stream::MarkdownStreamCollector;
    use std::hint::black_box;
    use std::time::{Duration, Instant};
    use table_holdback::TableHoldbackScanner;

    let iterations: usize = std::env::args().nth(1).unwrap().parse().unwrap();
    for sample in 0..=iterations {
        let start = Instant::now();
        for _ in 0..200 {
            let mut scanner = TableHoldbackScanner::new();
            for _ in 0..128 {
                scanner.push_source_chunk(black_box("ordinary | pipe | prose\n"));
            }
            black_box(scanner.state());
        }
        if sample != 0 { // one unreported warmup
            println!("{{\"scanner_25600_lines_us\":{}}}", start.elapsed().as_micros());
        }
    }

    let mut collector = MarkdownStreamCollector::new(None, &std::env::temp_dir());
    for _ in 0..100 {
        collector.push_delta("word ");
        assert!(collector.commit_complete_source().is_none());
    }
    collector.push_delta("\n");
    let committed = collector.commit_complete_source().unwrap();
    assert_eq!(committed, format!("{}\n", "word ".repeat(100)));
    collector.push_delta("remaining é");
    assert_eq!(collector.finalize_and_drain_source(), "remaining é\n");
    assert_eq!(collector.finalize_and_drain_source(), "");
    println!("{{\"unterminated_deltas\":100,\"commits_before_newline\":0,\"committed_bytes\":{}}}", committed.len());

    // Virtual age, not a wall-clock simulation of a functioning UI scheduler.
    let t0 = Instant::now();
    let mut policy = AdaptiveChunkingPolicy::default();
    policy.decide(QueueSnapshot { queued_lines: 8, oldest_age: Some(Duration::ZERO) }, t0);
    policy.decide(QueueSnapshot::default(), t0 + Duration::from_millis(1));
    let age = (0..=300).find(|age| {
        let decision = policy.decide(
            QueueSnapshot { queued_lines: 8, oldest_age: Some(Duration::from_millis(*age)) },
            t0 + Duration::from_millis(2 + age),
        );
        matches!(decision.drain_plan, DrainPlan::Batch(_))
    }).expect("catch-up must eventually drain");
    println!("{{\"post_burst_catch_up_age_ms\":{age}}}");
}
'''


def run(args: list[str], cwd: Path) -> str:
    result = subprocess.run(args, cwd=cwd, capture_output=True, text=True, check=False)
    if result.returncode:
        raise RuntimeError(f"{args!r} exited {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result.stdout


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iterations", type=int, default=9)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.iterations < 1:
        parser.error("--iterations must be positive")
    root = Path(__file__).resolve().parents[1]
    rust_root = root / "codex-rs"
    sources = [
        "tui/src/table_detect.rs", "tui/src/streaming/chunking.rs",
        "tui/src/streaming/table_holdback.rs", "tui/src/markdown_stream.rs",
    ]
    hashes = {}
    with tempfile.TemporaryDirectory(prefix="kd4-stream-bench-") as directory:
        work = Path(directory)
        for source in sources:
            data = (rust_root / source).read_bytes()
            hashes[source] = hashlib.sha256(data).hexdigest()
            if source == "tui/src/table_detect.rs":
                # Only the test module needs pulldown_cmark. Do not substitute
                # its parser or copy a second implementation of production code.
                marker = b"#[cfg(test)]\nmod tests {"
                if data.count(marker) != 1:
                    raise RuntimeError("table_detect test boundary changed")
                data = data.replace(marker, b"#[cfg(any())]\nmod tests {")
            (work / Path(source).name).write_bytes(data)
        harness = work / "main.rs"
        harness.write_text(HARNESS, encoding="utf-8")
        binary = work / ("probe.exe" if os.name == "nt" else "probe")
        tests = work / ("tests.exe" if os.name == "nt" else "tests")
        compiler = run(["rustc", "--version"], rust_root).strip()
        run(["rustc", "--edition=2024", "-O", str(harness), "-o", str(binary)], rust_root)
        records = [json.loads(line) for line in run([str(binary), str(args.iterations)], rust_root).splitlines()]
        run(["rustc", "--edition=2024", "--test", str(harness), "-o", str(tests)], rust_root)
        test_output = run([str(tests)], rust_root)
    timings = [row["scanner_25600_lines_us"] for row in records if "scanner_25600_lines_us" in row]
    result = {
        "scope": "isolated production primitives; tracing disabled; no UI/network timing",
        "compiler": compiler,
        "source_sha256": hashes,
        "excluded_tests": ["table_detect renderer integration and markdown_stream rendering tests"],
        "scanner_us": {"samples": timings, "median": statistics.median(timings), "min": min(timings), "max": max(timings)},
        "boundary_probes": records[len(timings):],
        "primitive_test_output": test_output,
    }
    output = json.dumps(result, indent=2)
    if args.output:
        args.output.write_text(output + "\n", encoding="utf-8")
    print(output)


if __name__ == "__main__":
    main()
