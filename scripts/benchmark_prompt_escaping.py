"""Benchmark the exact std-only prompt escaping helper against its old implementation.

Uses the repository Rust toolchain, without Cargo, dependencies, or build lanes.
This proves local helper costs and byte equivalence, not end-to-end model latency.
"""

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import tempfile


HARNESS = r'''
use std::hint::black_box;
use std::time::Instant;

fn reference(text: &str, delimiters: &[&str]) -> String {
    delimiters.iter().fold(text.to_string(), |text, delimiter| {
        text.replace(delimiter, &delimiter.replace('<', "&lt;").replace('>', "&gt;"))
    })
}

fn measure(text: &str, delimiters: &[&str], f: fn(&str, &[&str]) -> String) -> u128 {
    let mut samples = Vec::new();
    for _ in 0..11 {
        let started = Instant::now();
        for _ in 0..1000 {
            black_box(f(black_box(text), black_box(delimiters)));
        }
        samples.push(started.elapsed().as_nanos() / 1000);
    }
    samples.sort_unstable();
    samples[5]
}

fn main() {
    let delimiters = ["<INSTRUCTIONS>", "</INSTRUCTIONS>", "<AGENTS_MD_OBSERVATION>", "</AGENTS_MD_OBSERVATION>"];
    let cases = [
        ("plain_small", "Follow the local instructions and preserve user work.".to_string()),
        ("plain_large", "Follow the local instructions and preserve user work.\n".repeat(600)),
        ("unicode_xml", "λ <node> &lt;tag&gt; preserve Unicode and source syntax.\n".repeat(600)),
        ("dense_delimiters", "<INSTRUCTIONS>λ</INSTRUCTIONS><AGENTS_MD_OBSERVATION>x</AGENTS_MD_OBSERVATION>".repeat(60)),
    ];
    for (name, text) in &cases {
        assert_eq!(escape_fragment_delimiters(text, &delimiters), reference(text, &delimiters));
        let old = measure(text, &delimiters, reference);
        let current = measure(text, &delimiters, escape_fragment_delimiters);
        println!("{} {} {} {}", name, text.len(), old, current);
    }
    // Preserve ordered replacement behavior, including overlapping and empty markers.
    for markers in [vec![], vec![""], vec!["<", "<INSTRUCTIONS>"], vec!["<INSTRUCTIONS>", "&lt;"], vec!["λ", "x"]] {
        for (_, text) in &cases {
            assert_eq!(escape_fragment_delimiters(text, &markers), reference(text, &markers));
        }
    }
}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=pathlib.Path)
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parents[1]
    source_path = root / "codex-rs/core/src/context/mod.rs"
    source = source_path.read_bytes()
    matches = re.findall(
        r"(?ms)^fn escape_fragment_delimiters\(.*?^}", source.decode("utf-8")
    )
    if len(matches) != 1:
        raise RuntimeError("expected exactly one complete escaping helper")
    rustc_version = subprocess.check_output(
        ["rustc", "--version"], cwd=root / "codex-rs", text=True
    ).strip()
    with tempfile.TemporaryDirectory(prefix="codex-prompt-escaping-") as temp:
        temp = pathlib.Path(temp)
        benchmark = temp / "benchmark.rs"
        executable = temp / "benchmark.exe"
        benchmark.write_text(matches[0] + "\n" + HARNESS, encoding="utf-8")
        subprocess.run(
            ["rustc", "--edition=2024", "-O", str(benchmark), "-o", str(executable)],
            cwd=root / "codex-rs", check=True,
        )
        output = subprocess.check_output([str(executable)], text=True)
    measurements = []
    for line in output.splitlines():
        name, size, baseline, current = line.split()
        measurements.append({
            "case": name, "input_bytes": int(size),
            "baseline_median_ns": int(baseline), "current_median_ns": int(current),
        })
    result = {
        "scope": "exact production escaping helper, isolated optimized Rust microbenchmark",
        "rustc": rustc_version,
        "source": source_path.relative_to(root).as_posix(),
        "source_sha256": hashlib.sha256(source).hexdigest(),
        "samples": 11, "iterations_per_sample": 1000,
        "byte_equivalence": "passed", "measurements": measurements,
    }
    rendered = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.write_text(rendered, encoding="utf-8")
    print(rendered, end="")


if __name__ == "__main__":
    main()
