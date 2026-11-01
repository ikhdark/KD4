fn main() {
    println!("cargo:rerun-if-changed=src/assets/samples");
    let samples_dir = std::path::Path::new("src/assets/samples");
    assert!(
        samples_dir.is_dir(),
        "bundled skills directory src/assets/samples is missing"
    );
    let bundled_dir =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"))
            .join("bundled-skills");
    match std::fs::remove_dir_all(&bundled_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to replace staged bundled skills: {error}"),
    }
    stage_bundled_skills(samples_dir, &bundled_dir).expect("failed to stage bundled skills");
}

fn stage_bundled_skills(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        if source.is_dir() {
            // Local Python runs must not change the embedded product or its fingerprint.
            if entry.file_name() != "__pycache__" {
                stage_bundled_skills(&source, &destination)?;
            }
        } else if source.is_file() {
            std::fs::write(&destination, std::fs::read(&source)?)?;
        } else {
            return Err(std::io::Error::other(format!(
                "bundled skill entry is neither a file nor a directory: {}",
                source.display()
            )));
        }
    }
    Ok(())
}
