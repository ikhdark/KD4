//! The integration-test harness, compiled into codex-core's unit tests.
//!
//! `core_test_support` depends on codex-core, so as a dev-dependency it forced a
//! second, normal codex-core build before every unit-test build. Unit tests use
//! its core-independent half (`core_test_support_base`) directly and compile its
//! core-dependent files here, against this crate.

// Integration tests use the whole harness; codex-core's unit tests use part of it.
#![allow(dead_code)]

use ctor::ctor;

pub(crate) use core_test_support_base::*;

#[path = "../tests/common/apps_test_server.rs"]
pub(crate) mod apps_test_server;
#[path = "../tests/common/context_snapshot.rs"]
pub(crate) mod context_snapshot;
#[path = "../tests/common/hooks.rs"]
pub(crate) mod hooks;
#[path = "../tests/common/session_helpers.rs"]
mod session_helpers;
#[path = "../tests/common/test_codex.rs"]
pub(crate) mod test_codex;

pub(crate) use session_helpers::*;

// `core_test_support` sets these for integration tests; unit tests no longer link it.
#[ctor(unsafe)]
fn enable_deterministic_unified_exec_process_ids_for_tests() {
    crate::test_support::set_thread_manager_test_mode(/*enabled*/ true);
    crate::test_support::set_deterministic_process_ids(/*enabled*/ true);
}

#[test]
fn require_network_checks_the_spawn_sandbox_marker() {
    assert_eq!(
        sandbox_network_env_var(),
        crate::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
    );
}
