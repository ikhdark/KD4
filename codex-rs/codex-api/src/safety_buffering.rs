use crate::common::SafetyBufferingTreatment;
use http::HeaderMap;

pub(crate) const X_CODEX_SAFETY_BUFFERING_ENABLED_HEADER: &str = "x-codex-safety-buffering-enabled";
pub(crate) const X_CODEX_SAFETY_BUFFERING_FASTER_MODEL_HEADER: &str =
    "x-codex-safety-buffering-faster-model";

pub(crate) fn treatment_from_headers(headers: &HeaderMap) -> Option<SafetyBufferingTreatment> {
    if !headers.contains_key(X_CODEX_SAFETY_BUFFERING_ENABLED_HEADER)
        && !headers.contains_key(X_CODEX_SAFETY_BUFFERING_FASTER_MODEL_HEADER)
    {
        return None;
    }
    let faster_model = headers
        .get(X_CODEX_SAFETY_BUFFERING_FASTER_MODEL_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    Some(SafetyBufferingTreatment { faster_model })
}

