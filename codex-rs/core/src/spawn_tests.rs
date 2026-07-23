use super::*;
use pretty_assertions::assert_eq;
use std::sync::Mutex;
use tracing::Level;
use tracing_test::internal::MockWriter;

#[test]
fn enabled_network_policy_removes_inherited_disabled_marker() {
    let mut env = HashMap::from([
        (
            CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR.to_string(),
            "stale".to_string(),
        ),
        ("KEEP_ME".to_string(), "value".to_string()),
    ]);

    apply_network_sandbox_policy_to_env(&mut env, NetworkSandboxPolicy::Enabled);

    assert_eq!(env.get(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR), None);
    assert_eq!(env.get("KEEP_ME").map(String::as_str), Some("value"));
}

#[cfg(windows)]
#[test]
fn enabled_network_policy_removes_differently_cased_disabled_marker() {
    let mut env = HashMap::from([(
        CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR.to_ascii_lowercase(),
        "stale".to_string(),
    )]);

    apply_network_sandbox_policy_to_env(&mut env, NetworkSandboxPolicy::Enabled);

    assert!(
        env.keys()
            .all(|key| !key.eq_ignore_ascii_case(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR))
    );
}

#[test]
fn restricted_network_policy_replaces_inherited_disabled_marker() {
    let mut env = HashMap::from([(
        CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR.to_string(),
        "stale".to_string(),
    )]);

    apply_network_sandbox_policy_to_env(&mut env, NetworkSandboxPolicy::Restricted);

    assert_eq!(
        env.get(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR)
            .map(String::as_str),
        Some("1")
    );
}

#[test]
fn spawn_trace_omits_environment_names_and_values() {
    let buffer: &'static Mutex<Vec<u8>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(Level::TRACE)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let cwd = AbsolutePathBuf::try_from(std::env::current_dir().expect("current directory"))
        .expect("absolute current directory");
    let env = HashMap::from([(
        "PHASE_73_SECRET_NAME".to_string(),
        "phase-73-secret-value".to_string(),
    )]);

    trace_spawn_child(
        &PathBuf::from("test-program"),
        &["--flag".to_string()],
        None,
        &cwd,
        NetworkSandboxPolicy::Enabled,
        StdioPolicy::RedirectForShellTool,
        &env,
    );

    let logs = String::from_utf8(buffer.lock().expect("buffer lock").clone()).expect("utf8 logs");
    assert!(logs.contains("env_count=1"));
    assert!(!logs.contains("PHASE_73_SECRET_NAME"));
    assert!(!logs.contains("phase-73-secret-value"));
}
