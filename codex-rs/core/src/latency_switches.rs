use std::sync::OnceLock;

fn enabled(name: &'static str, slot: &'static OnceLock<bool>) -> bool {
    *slot.get_or_init(|| {
        !std::env::var(name).is_ok_and(|value| value.eq_ignore_ascii_case("baseline"))
    })
}

pub(crate) fn stage2_critical_path_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    enabled("CODEX_LATENCY_STAGE2", &ENABLED)
}

pub(crate) fn stage3_persistence_history_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    enabled("CODEX_LATENCY_STAGE3", &ENABLED)
}

pub(crate) fn stage5_executor_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    enabled("CODEX_LATENCY_STAGE5", &ENABLED)
}

pub(crate) fn history_identity_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    enabled("CODEX_LATENCY_STAGE_HISTORY_IDENTITY", &ENABLED)
}

pub(crate) fn shared_prompt_input_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    enabled("CODEX_LATENCY_STAGE_SHARED_PROMPT_INPUT", &ENABLED)
}
