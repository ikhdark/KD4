pub(crate) fn handle_exit_status(status: std::process::ExitStatus) -> ! {
    if let Some(code) = status.code() {
        std::process::exit(code);
    } else {
        // Rare on Windows, but if it happens: use fallback code.
        std::process::exit(1);
    }
}
