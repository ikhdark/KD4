fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(codex_code_mode_host::run_stdio());
    // Tokio's standard streams can retain uncancellable blocking operations.
    // This dedicated child must also bound the final runtime teardown.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    result
}
