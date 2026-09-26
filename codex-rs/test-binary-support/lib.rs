pub use codex_arg0::Arg0PathEntryGuard;
use codex_arg0::arg0_dispatch;
use codex_arg0::arg0_dispatch_helper;

/// Configures arg0 dispatch for a test binary. Call it from a `#[ctor]`.
///
/// A helper re-execution dispatches with its inherited environment and never
/// returns. Any other process gets PATH aliases rooted in a CODEX_HOME shared by
/// every process of the named test binary family, never the developer's home.
pub fn configure_test_binary_dispatch(codex_home_dir_name: &str) -> Option<Arg0PathEntryGuard> {
    // The helper must observe the caller's CODEX_HOME, not the alias home.
    arg0_dispatch_helper();

    // Guards live in `#[ctor]` statics, which are never dropped, so a per-process
    // temporary home would leak on every test process. A shared home lets the arg0
    // janitor remove alias directories whose owning process has exited.
    let codex_home = std::env::temp_dir().join(codex_home_dir_name);
    if let Err(error) = std::fs::create_dir_all(&codex_home) {
        panic!("failed to create test CODEX_HOME: {error}");
    }
    let previous_codex_home = std::env::var_os("CODEX_HOME");
    // Safety: this runs from a test ctor before test threads begin.
    unsafe {
        std::env::set_var("CODEX_HOME", &codex_home);
    }

    let arg0 = match arg0_dispatch() {
        Some(arg0) => arg0,
        None => panic!("failed to configure arg0 dispatch aliases for test binary"),
    };
    match previous_codex_home.as_ref() {
        Some(value) => unsafe {
            // SAFETY: the test ctor is still running before test threads start.
            std::env::set_var("CODEX_HOME", value);
        },
        None => unsafe {
            // SAFETY: the test ctor is still running before test threads start.
            std::env::remove_var("CODEX_HOME");
        },
    }

    Some(arg0)
}
