use std::fmt::Write;
use std::path::PathBuf;

// Canonical implementations remain in scripts/. Embed their complete local import
// closure, not a second maintained copy or a runtime dependency on this checkout.
const HARNESS_SCRIPTS: &[&str] = &[
    "atomic_json.py",
    "source_inventory.py",
    "tool_result_audit.py",
    "validation_metrics.py",
    "rollout_snapshot.py",
    "rollout_audit_cache.py",
    "kd4_turn_latency_audit.py",
    "kd4_timing_analysis.py",
    "kd4_session_diagnostics.py",
    "kd4_first_useful_action_analysis.py",
];

fn main() {
    println!("cargo:rerun-if-changed=src/assets/samples");
    let samples_dir = std::path::Path::new("src/assets/samples");
    assert!(
        samples_dir.is_dir(),
        "bundled skills directory src/assets/samples is missing"
    );
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let mut embedded = String::from("&[\n");
    for name in HARNESS_SCRIPTS {
        let source = manifest.join("../../scripts").join(name);
        assert!(
            source.is_file(),
            "missing harness script: {}",
            source.display()
        );
        println!("cargo:rerun-if-changed={}", source.display());
        let destination = format!("harness-tools/scripts/lib/{name}");
        writeln!(
            embedded,
            "({destination:?}, include_bytes!({source:?}) as &[u8]),"
        )
        .expect("format embedded script");
    }
    embedded.push_str("]\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("build output dir"));
    std::fs::write(out.join("harness_tool_scripts.rs"), embedded)
        .expect("write embedded harness script manifest");
}
