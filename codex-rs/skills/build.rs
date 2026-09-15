fn main() {
    println!("cargo:rerun-if-changed=src/assets/samples");
    let samples_dir = std::path::Path::new("src/assets/samples");
    assert!(
        samples_dir.is_dir(),
        "bundled skills directory src/assets/samples is missing"
    );
}
