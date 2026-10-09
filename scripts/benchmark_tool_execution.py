#!/usr/bin/env python3
"""Benchmark the production tokenizer without a model or a workspace Cargo build.

Compiles immutable source copies against an existing tiktoken rlib. Measures
projection and a child-process -> collection -> projection pipeline separately.
The pipeline is not the app-server/JS dispatcher; use its owning integration
tests for that boundary. No baseline is inferred from repository history.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile


HARNESS = r'''
#![allow(dead_code)]
mod tokenizer;
mod baseline_tokenizer;
use std::hint::black_box;
use std::time::Instant;
const CELL_PRECHECK: bool = false;

#[cfg(windows)]
fn cpu_us() -> u64 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn GetProcessTimes(handle: *mut std::ffi::c_void, created: *mut u64,
            exited: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
    }
    let (mut created, mut exited, mut kernel, mut user) = (0, 0, 0, 0);
    // FILETIME consists of two little-endian DWORDs. These aligned u64 buffers
    // are valid eight-byte output storage for the duration of the native call.
    assert_ne!(unsafe { GetProcessTimes(GetCurrentProcess(), &mut created,
        &mut exited, &mut kernel, &mut user) }, 0);
    (kernel + user) / 10
}

#[cfg(not(windows))]
fn cpu_us() -> u64 { 0 }

fn fixture(name: &str) -> String {
    (match name {
        "source" => "    let result = read_source_file(path).await?;\n".repeat(2_000),
        "search" => "src/first.rs:12:needle\nC:\\repo\\second.rs:3:needle\n".repeat(2_000),
        "unicode" => "🙂漢字 source evidence\r\n".repeat(2_000),
        _ => panic!("unknown fixture"),
    }) + "FAILURE_TAIL\n"
}

fn project(text: &str, limit: usize, baseline: bool) -> (String, Option<(usize, usize)>) {
    if CELL_PRECHECK {
        let whole = limit.saturating_add(limit / 3).min(40_000);
        if !baseline {
            return tokenizer::truncate_model_text_at_lines_with_limits(
                text, limit, whole, 0, text.lines().count(), Some("fixture-artifact"),
            );
        }
        if limit != 0 && baseline_tokenizer::model_token_count(text) <= whole {
            return (text.to_string(), None);
        }
    }
    let project = if baseline { baseline_tokenizer::truncate_model_text_at_lines_with_recovery }
        else { tokenizer::truncate_model_text_at_lines_with_recovery };
    project(
        text, limit, 0, text.lines().count(), Some("fixture-artifact"),
    )
}

#[test]
fn small_budget_differential_preserves_diagnostics() {
    for line in ["source evidence\r\n", "src/file.rs:12:needle\n", "🙂漢字 source evidence\n"] {
        let source = line.repeat(64) + "FAILURE_TAIL\n";
        for limit in 0..=256 {
            let (before, _) = project(&source, limit, true);
            let (after, _) = project(&source, limit, false);
            assert!(before.is_empty() || !after.is_empty(), "lost packet at {limit}: {line}");
            assert!(!before.ends_with("FAILURE_TAIL\n") || after.ends_with("FAILURE_TAIL\n"),
                "lost failure tail at {limit}: {line}");
            let ceiling = if CELL_PRECHECK && !after.is_empty() && after == source {
                limit.saturating_add(limit / 3)
            } else { limit };
            assert!(tokenizer::model_token_count(&after) <= ceiling);
            if CELL_PRECHECK { assert_eq!(project(&source, limit, true), project(&source, limit, false)); }
        }
    }
    for name in ["source", "search", "unicode"] {
        let source = fixture(name);
        assert_eq!(project(&source, 10_000, true), project(&source, 10_000, false));
    }
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--emit") {
        print!("{}", fixture(&args[2]));
        return;
    }
    let samples: usize = args[1].parse().unwrap();
    if let Some(path) = args.get(2) {
        // Length-prefixed UTF-8 avoids measuring an unrelated JSON parser.
        let bytes = std::fs::read(path).unwrap();
        let mut rest = bytes.as_slice();
        let mut corpus = Vec::new();
        while !rest.is_empty() {
            let size = u64::from_le_bytes(rest[..8].try_into().unwrap()) as usize;
            corpus.push(std::str::from_utf8(&rest[8..8 + size]).unwrap());
            rest = &rest[8 + size..];
        }
        for text in &corpus { assert_eq!(project(text, 10_000, true), project(text, 10_000, false)); }
        for sample in 0..samples {
            for baseline in if sample % 2 == 0 { [true, false] } else { [false, true] } {
                let cpu_start = cpu_us();
                let start = Instant::now();
                let mut output_bytes = 0;
                for text in &corpus { output_bytes += black_box(project(text, 10_000, baseline)).0.len(); }
                let micros = start.elapsed().as_micros();
                let cpu = cpu_us() - cpu_start;
                println!("corpus,recorded_packets,10000,{baseline},{micros},{},{output_bytes},{cpu}", corpus.len());
            }
        }
    }
    for name in ["source", "search", "unicode"] {
        let source = fixture(name);
        for limit in [100, 10_000] {
            for sample in 0..=samples {
              for baseline in if sample % 2 == 0 { [true, false] } else { [false, true] } {
                let cpu_start = cpu_us();
                let start = Instant::now();
                let (output, gap) = project(black_box(&source), limit, baseline);
                let micros = start.elapsed().as_micros();
                let cpu = cpu_us() - cpu_start;
                let iterations = if baseline { baseline_tokenizer::take_iterations() }
                    else { tokenizer::take_iterations() };
                let whole = limit.saturating_add(limit / 3).min(40_000);
                let fits_whole = CELL_PRECHECK && tokenizer::model_token_count(&source) <= whole;
                assert!(tokenizer::model_token_count(&output) <= if fits_whole { whole } else { limit });
                assert!(output.ends_with("FAILURE_TAIL\n"));
                assert_eq!(gap.is_none(), fits_whole);
                assert_eq!(project(&source, limit, true), project(&source, limit, false));
                if sample > 0 {
                    println!("projection,{name},{limit},{baseline},{micros},{iterations},{},{cpu}", output.len());
                }
              }
            }
        }
        for sample in 0..=samples {
          for baseline in if sample % 2 == 0 { [true, false] } else { [false, true] } {
            let start = Instant::now();
            let child_start = Instant::now();
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--emit", name]).output().unwrap();
            let child_us = child_start.elapsed().as_micros();
            assert!(child.status.success());
            assert!(child.stderr.is_empty());
            let collected = String::from_utf8(child.stdout).unwrap();
            assert_eq!(collected, source);
            let cpu_start = cpu_us();
            let (output, gap) = project(&collected, 100, baseline);
            let micros = start.elapsed().as_micros();
            let cpu = cpu_us() - cpu_start;
            if baseline { baseline_tokenizer::take_iterations(); } else { tokenizer::take_iterations(); }
            assert!(tokenizer::model_token_count(&output) <= 100);
            assert!(output.ends_with("FAILURE_TAIL\n"));
            assert!(gap.is_some());
            if sample > 0 {
                println!("process_to_projection,{name},100,{baseline},{micros},{child_us},{},{cpu}", output.len());
            }
          }
        }
    }
}
'''


def run(command: list[str], cwd: Path) -> str:
    result = subprocess.run(command, cwd=cwd, capture_output=True, encoding="utf-8", check=False)
    if result.returncode:
        raise RuntimeError(f"{command!r} exited {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result.stdout


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--deps", type=Path, required=True, help="Existing Cargo dependency directory")
    parser.add_argument("--source-dir", type=Path, help="Captured tokenizer.rs and lib.rs; defaults to current source")
    parser.add_argument("--baseline-dir", type=Path, help="Alternate samples against this immutable tokenizer baseline")
    parser.add_argument("--capture", type=Path, help="Create an immutable baseline source directory, then benchmark it")
    parser.add_argument("--samples", type=int, default=7)
    parser.add_argument("--cell-precheck", action="store_true", help="Compare the prior count-then-truncate cell path with single-pass admission")
    parser.add_argument("--corpus", type=Path, help="JSON array of recorded packets with text fields; replay at a controlled 10000-token budget")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.corpus and not args.cell_precheck:
        parser.error("--corpus requires --cell-precheck")
    if not 1 <= args.samples <= 100:
        parser.error("samples must be 1–100")
    root = Path(__file__).resolve().parents[1]
    source_dir = args.source_dir or root / "codex-rs/utils/output-truncation/src"
    sources = {name: (source_dir / name).read_bytes() for name in ("tokenizer.rs", "lib.rs")}
    baseline = (args.baseline_dir / "tokenizer.rs").read_bytes() if args.baseline_dir else sources["tokenizer.rs"]
    if args.capture:
        args.capture.mkdir(parents=True, exist_ok=False)
        for name, data in sources.items():
            (args.capture / name).write_bytes(data)
    deps = args.deps.resolve()
    tiktoken = list(deps.glob("libtiktoken_rs-*.rlib"))
    if len(tiktoken) != 1:
        parser.error(f"expected one tiktoken rlib, found {len(tiktoken)}")
    lib = sources["lib.rs"].decode("utf-8")
    start = lib.index("fn omitted_line_marker_at_lines(")
    end = lib.index("const OMITTED_RANGE_MAX_LINES:", start)
    helpers = lib[start:end]
    with tempfile.TemporaryDirectory(prefix="kd4-tool-execution-bench-") as directory:
        work = Path(directory)
        for name, data in [("tokenizer.rs", sources["tokenizer.rs"]), ("baseline_tokenizer.rs", baseline)]:
            text = data.decode("utf-8")
            if name == "tokenizer.rs" and "pub fn truncate_model_text_at_lines_with_limits(" not in text:
                if args.cell_precheck:
                    parser.error("--cell-precheck requires the single-pass tokenizer source")
                # Keep historical --source-dir snapshots usable in legacy mode.
                text += "\npub fn truncate_model_text_at_lines_with_limits(text: &str, limit: usize, _whole: usize, offset: usize, total: usize, artifact: Option<&str>) -> (String, Option<(usize, usize)>) { truncate_model_text_at_lines_with_recovery(text, limit, offset, total, artifact) }\n"
            assert text.count("    loop {") == 1, "fitting-loop instrumentation boundary changed"
            text = text.replace("    loop {", "    loop {\n        ITERATIONS.with(|n| n.set(n.get() + 1));")
            text += "\nthread_local! { static ITERATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }\n"
            text += "pub fn take_iterations() -> usize { ITERATIONS.with(|n| n.replace(0)) }\n"
            (work / name).write_text(text, encoding="utf-8")
        harness = HARNESS.replace("const CELL_PRECHECK: bool = false;",
                                  f"const CELL_PRECHECK: bool = {str(args.cell_precheck).lower()};")
        corpus_info = None
        corpus_args = []
        if args.corpus:
            data = args.corpus.read_bytes()
            packets = json.loads(data)
            if not isinstance(packets, list) or not packets or not all(
                isinstance(packet, dict) and isinstance(packet.get("text"), str) for packet in packets
            ):
                parser.error("corpus must be a nonempty array of objects with text fields")
            corpus_path = work / "corpus.bin"
            with corpus_path.open("wb") as stream:
                for packet in packets:
                    raw = packet["text"].encode("utf-8")
                    stream.write(len(raw).to_bytes(8, "little"))
                    stream.write(raw)
            corpus_info = {"sha256": hashlib.sha256(data).hexdigest(), "packets": len(packets),
                           "note": "Recorded displayed packets, not recovered original pre-projection input; controlled 10000-token replay, not historical saved time."}
            corpus_args = [str(corpus_path)]
        (work / "main.rs").write_text(harness + helpers, encoding="utf-8")
        compile_args = ["rustc", "--edition=2024", "-O", "-C", "target-feature=+crt-static",
                        "-L", f"dependency={deps}", "--extern", f"tiktoken_rs={tiktoken[0]}"]
        if linker := shutil.which("lld-link"):
            compile_args.extend(["-C", f"linker={linker}"])
        binary = work / ("probe.exe" if os.name == "nt" else "probe")
        compiler = run(["rustc", "--version"], root / "codex-rs").strip()
        run(compile_args + [str(work / "main.rs"), "-o", str(binary)], root / "codex-rs")
        records = run([str(binary), str(args.samples), *corpus_args], root / "codex-rs").splitlines()
        tests = work / ("tests.exe" if os.name == "nt" else "tests")
        run(compile_args + ["--test", str(work / "main.rs"), "-o", str(tests)], root / "codex-rs")
        tests_output = run([str(tests)], work)
    groups: dict[str, list[int]] = {}
    for row in records:
        fields = row.split(",")
        groups.setdefault("/".join(fields[:4]), []).append(int(fields[4]))
    report = {
        "scope": "Production-source projection and child-process-to-projection; no model, app-server or JS dispatch",
        "compiler": compiler,
        "source_sha256": {k: hashlib.sha256(v).hexdigest() for k, v in sources.items()},
        "baseline_tokenizer_sha256": hashlib.sha256(baseline).hexdigest(),
        "variant": "true=baseline; false=candidate; alternating order within each sample",
        "record_fields": ["phase", "fixture", "token_limit", "baseline", "wall_us", "iterations_or_child_wall_us", "output_bytes", "projection_process_cpu_us"],
        "cpu_note": "Windows process CPU uses GetProcessTimes (coarse quantization); wall minus CPU is not a pure scheduler measurement. Child wall includes creation, execution and pipe collection. CPU is zero/unavailable on other platforms.",
        "tiktoken_rlib_sha256": hashlib.sha256(tiktoken[0].read_bytes()).hexdigest(),
        "samples": args.samples,
        "cell_precheck": args.cell_precheck,
        "corpus": corpus_info,
        "timings_us": {k: {"samples": v, "median": statistics.median(v)} for k, v in groups.items()},
        "records": records,
        "tests": tests_output,
    }
    with args.output.open("x", encoding="utf-8") as stream:
        json.dump(report, stream, indent=2)
        stream.write("\n")
    print(json.dumps({"report": str(args.output), "medians_us": {k: v["median"] for k, v in report["timings_us"].items()}, "tests": tests_output}))


if __name__ == "__main__":
    main()
