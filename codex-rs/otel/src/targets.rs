pub(crate) const OTEL_TARGET_PREFIX: &str = "codex_otel";
pub(crate) const OTEL_LOG_ONLY_TARGET: &str = "codex_otel.log_only";
pub(crate) const OTEL_TRACE_SAFE_TARGET: &str = "codex_otel.trace_safe";

pub(crate) fn is_log_export_target(target: &str) -> bool {
    (target == OTEL_TARGET_PREFIX
        || target.starts_with("codex_otel.")
        || target.starts_with("codex_otel::"))
        && !is_trace_safe_target(target)
}

pub(crate) fn is_trace_safe_target(target: &str) -> bool {
    target == OTEL_TRACE_SAFE_TARGET || target.starts_with("codex_otel.trace_safe.")
}
