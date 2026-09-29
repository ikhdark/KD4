//! Opt-in component measurements. Never interpret these as live-model savings.
use serde_json::Value;
use serde_json::json;
use std::hint::black_box;
use std::io::Write;
use std::time::Instant;

pub(crate) fn tokens(text: &str) -> usize {
    codex_utils_output_truncation::model_token_count(text)
}

pub(crate) fn summarize(mut samples: Vec<u64>) -> Value {
    let raw = samples.clone();
    samples.sort_unstable();
    json!({"samples_ns":raw,"median_ns":samples[samples.len()/2],
        "p95_ns":samples[(samples.len()*95/100).min(samples.len()-1)]})
}

pub(crate) fn measure<T>(mut operation: impl FnMut() -> T) -> Value {
    for _ in 0..3 {
        black_box(operation());
    }
    let mut samples = Vec::new();
    for _ in 0..21 {
        let start = Instant::now();
        for _ in 0..8 {
            black_box(operation());
        }
        samples.push(u64::try_from(start.elapsed().as_nanos() / 8).unwrap());
    }
    summarize(samples)
}

#[allow(clippy::print_stdout)]
pub(crate) fn emit(finding: u32, case: &str, measurements: Value) {
    let record = json!({"finding":finding,"case":case,
        "scope":"local component; no model request or synthesis quality measured",
        "measurements":measurements});
    if let Some(path) = std::env::var_os("GENERATION_BENCH_OUTPUT") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{record}").unwrap();
    }
    println!("GENERATION_BENCH {record}");
}
