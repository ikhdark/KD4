//! Bounded journal pressure benchmark: inert numeric metadata, no app or models.
use codex_protocol::timing::observations;
use serde_json::json;
use std::time::Instant;

fn main() {
    let started = Instant::now();
    let small = std::env::args().any(|arg| arg == "--small-rows");
    let stress_mobile = std::env::args().any(|arg| arg == "--stress-mobile");
    let stress_lifecycle = std::env::args().any(|arg| arg == "--stress-lifecycle");
    let block_secondary = std::env::args().any(|arg| arg == "--block-secondary");
    if block_secondary {
        let directory = std::path::PathBuf::from(std::env::var_os("CODEX_RUNTIME_OBSERVATIONS_DIR").expect("benchmark output directory"));
        for lane in ["mobile", "lifecycle"] {
            std::fs::OpenOptions::new().write(true).create_new(true)
                .open(directory.join(format!("runtime-{}-{}-{lane}.jsonl", std::process::id(),
                    codex_protocol::timing::process_clock_domain_id()))).expect("fresh benchmark collision marker");
        }
    }
    let ids = if small { 0 } else { 65536 };
    let batch_limit = if small { 300000 } else { 300 };
    let mut accepted = 0;
    // Same existing public API in baseline and candidate; exhaust the ordinary
    // journal with realistic-sized metadata rows, not a missing-interface error.
    let metadata = json!({"counter_ids": vec![0_u32; ids]});
    for index in 0..batch_limit {
        if observations::record("journal_pressure", "", "", &format!("batch-{index}"), metadata.clone()).is_none() {
            break;
        }
        accepted += 1;
    }
    let ordinary_after_cap = observations::record("journal_pressure", "", "", "after-cap", json!({}));
    let mobile_after_cap = observations::record("mobile_rpc_received", "", "", "pressure-request",
        json!({"schema_version":1,"connection_id":1,"method":"thread/resume"}));
    let mut mobile_accepted = 0;
    if stress_mobile {
        for index in 0..batch_limit {
            if observations::record("mobile_pressure", "", "", &format!("batch-{index}"), metadata.clone()).is_none() {
                break;
            }
            mobile_accepted += 1;
        }
    }
    let retirement_after_cap = observations::record("mobile_connection_retired", "", "", "connection-1",
        json!({"schema_version":1,"connection_id":1,"reason":"peer_closed"}));
    let mut retirements_accepted = 0;
    if stress_lifecycle {
        for index in 0..1000 {
            if observations::record("mobile_connection_retired", "", "", &format!("connection-{index}"),
                json!({"schema_version":1,"connection_id":index,"reason":"peer_closed"})).is_none() {
                break;
            }
            retirements_accepted += 1;
        }
    }
    println!("{}", json!({"workload":"native-journal-pressure-v1", "batch_limit":batch_limit,
        "stress_lifecycle":stress_lifecycle,"retirements_accepted":retirements_accepted,"block_secondary":block_secondary,
        "numeric_ids_per_batch":ids, "stress_mobile":stress_mobile, "mobile_batches_accepted":mobile_accepted,
        "ordinary_batches_accepted":accepted,
        "ordinary_after_cap":ordinary_after_cap.is_some(), "mobile_after_cap":mobile_after_cap.is_some(),
        "retirement_after_cap":retirement_after_cap.is_some(), "wall_seconds":started.elapsed().as_secs_f64()}));
}
