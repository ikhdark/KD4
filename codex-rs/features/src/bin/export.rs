use std::io::Write;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, &codex_features::feature_registry_entries())?;
    output.write_all(b"\n")?;
    Ok(())
}
