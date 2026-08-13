use std::collections::HashSet;
use std::sync::Mutex;

use codex_utils_output_truncation::approx_token_count;
use sha2::Digest;
use sha2::Sha256;

const MAX_MODEL_VISIBLE_UNIT_IDENTITIES: usize = 16_384;
const UNIT_HASH_DOMAIN: &[u8] = b"codex.kd4.model-visible-unit.v1";

#[derive(Debug, Default)]
pub(crate) struct RootAmplificationTelemetry {
    state: Mutex<RootAmplificationState>,
}

#[derive(Debug, Default)]
struct RootAmplificationState {
    unit_identities: HashSet<[u8; 32]>,
    saturated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UniqueMaterialMeasurement {
    pub(crate) unique_new_tokens: Option<i64>,
    pub(crate) measurement_complete: bool,
    pub(crate) null_reason: Option<&'static str>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Kd4DispatchTelemetryContext {
    pub(crate) local_projected_occupancy: i64,
    pub(crate) effective_provider_occupancy: Option<i64>,
    pub(crate) estimate_basis: &'static str,
    pub(crate) estimate_complete: bool,
    pub(crate) soft_floor_status: &'static str,
    pub(crate) compactions: u64,
}

impl RootAmplificationTelemetry {
    /// Measures canonical model-visible units while retaining identities only.
    /// Payload bytes are used transiently for hashing and token estimation and
    /// are never stored in telemetry state.
    pub(crate) fn measure_units<'a>(
        &self,
        units: impl IntoIterator<Item = (&'static str, &'a [u8])>,
    ) -> UniqueMaterialMeasurement {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.saturated {
            return saturated_measurement();
        }

        let mut unique_new_tokens = 0_i64;
        for (kind, payload) in units {
            let mut hasher = Sha256::new();
            hasher.update(UNIT_HASH_DOMAIN);
            hasher.update((kind.len() as u64).to_be_bytes());
            hasher.update(kind.as_bytes());
            hasher.update((payload.len() as u64).to_be_bytes());
            hasher.update(payload);
            let identity: [u8; 32] = hasher.finalize().into();
            if state.unit_identities.contains(&identity) {
                continue;
            }
            if state.unit_identities.len() == MAX_MODEL_VISIBLE_UNIT_IDENTITIES {
                state.saturated = true;
                return saturated_measurement();
            }
            state.unit_identities.insert(identity);
            unique_new_tokens = unique_new_tokens.saturating_add(
                i64::try_from(approx_token_count(&String::from_utf8_lossy(payload)))
                    .unwrap_or(i64::MAX),
            );
        }

        UniqueMaterialMeasurement {
            unique_new_tokens: Some(unique_new_tokens),
            measurement_complete: true,
            null_reason: None,
        }
    }
}

fn saturated_measurement() -> UniqueMaterialMeasurement {
    UniqueMaterialMeasurement {
        unique_new_tokens: None,
        measurement_complete: false,
        null_reason: Some("model_visible_unit_identity_set_saturated"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_units_are_counted_once() {
        let telemetry = RootAmplificationTelemetry::default();
        let first = telemetry.measure_units([("input", b"same".as_slice())]);
        let second = telemetry.measure_units([("input", b"same".as_slice())]);
        assert!(first.unique_new_tokens.is_some_and(|tokens| tokens > 0));
        assert_eq!(second.unique_new_tokens, Some(0));
    }

    #[test]
    fn saturation_is_sticky_and_does_not_evict() {
        let telemetry = RootAmplificationTelemetry::default();
        {
            let mut state = telemetry.state.lock().unwrap();
            for index in 0..MAX_MODEL_VISIBLE_UNIT_IDENTITIES {
                let mut identity = [0_u8; 32];
                identity[..8].copy_from_slice(&(index as u64).to_be_bytes());
                state.unit_identities.insert(identity);
            }
        }
        let saturated = telemetry.measure_units([("input", b"new".as_slice())]);
        let repeated = telemetry.measure_units([("input", b"new".as_slice())]);
        assert_eq!(
            saturated.null_reason,
            Some("model_visible_unit_identity_set_saturated")
        );
        assert_eq!(repeated, saturated);
        assert_eq!(
            telemetry.state.lock().unwrap().unit_identities.len(),
            MAX_MODEL_VISIBLE_UNIT_IDENTITIES
        );
    }
}
