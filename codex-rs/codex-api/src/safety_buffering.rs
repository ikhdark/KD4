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

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;

    #[test]
    fn treatment_depends_on_header_presence_not_enabled_value() {
        for enabled in [None, Some("true"), Some("false")] {
            for faster_model in [None, Some("faster-model")] {
                let mut headers = HeaderMap::new();
                if let Some(enabled) = enabled {
                    headers.insert(
                        X_CODEX_SAFETY_BUFFERING_ENABLED_HEADER,
                        HeaderValue::from_static(enabled),
                    );
                }
                if let Some(faster_model) = faster_model {
                    headers.insert(
                        X_CODEX_SAFETY_BUFFERING_FASTER_MODEL_HEADER,
                        HeaderValue::from_static(faster_model),
                    );
                }
                assert_eq!(
                    treatment_from_headers(&headers),
                    (enabled.is_some() || faster_model.is_some()).then(|| SafetyBufferingTreatment {
                        faster_model: faster_model.map(str::to_owned),
                    }),
                );
            }
        }
    }
}
