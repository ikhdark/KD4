use crate::plan_tool::ValidationRoute;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

pub const VALIDATION_CONTRACT_VERSION: u32 = 1;

/// Semantic proof applicability. Execution-instance metadata is deliberately
/// absent so this value can be used for both singleflight and completed reuse.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ValidationProofKey {
    pub repository: String,
    pub cwd: String,
    pub canonical_route_hash: String,
    pub implementation_identity: String,
    pub coverage_identity: String,
    pub environment_identity: String,
    pub toolchain_identity: String,
    pub configuration_identity: String,
    pub validation_contract_version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum ValidationTerminalStatus {
    Succeeded,
    #[serde(
        alias = "timed_out",
        alias = "cancelled",
        alias = "infrastructure_failure",
        alias = "admission_rejected"
    )]
    Failed,
    Superseded,
}

impl ValidationTerminalStatus {
    pub fn is_success(self) -> bool {
        self == Self::Succeeded
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum ValidationFreshness {
    Executed,
    Joined,
    Reused,
    Superseded,
}

/// Settled validation evidence. IDs, timing, and artifact integrity describe
/// one execution and therefore never participate in `ValidationProofKey`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ValidationResult {
    pub proof_key: ValidationProofKey,
    pub route: ValidationRoute,
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub process_id: Option<String>,
    pub status: ValidationTerminalStatus,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub failure_excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub failure_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub selected_test_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub raw_artifact_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub raw_artifact_sha256: Option<String>,
    pub freshness: ValidationFreshness,
}

#[cfg(test)]
mod tests {
    use super::ValidationResult;
    use super::ValidationTerminalStatus;

    #[test]
    fn terminal_status_wire_contract_is_exact_and_historical_failures_are_readable() {
        let statuses = [
            ValidationTerminalStatus::Succeeded,
            ValidationTerminalStatus::Failed,
            ValidationTerminalStatus::Superseded,
        ];
        assert_eq!(
            statuses
                .into_iter()
                .map(|status| serde_json::to_value(status).expect("status serialization"))
                .collect::<Vec<_>>(),
            vec![
                serde_json::json!("succeeded"),
                serde_json::json!("failed"),
                serde_json::json!("superseded"),
            ]
        );
        for historical in [
            "timed_out",
            "cancelled",
            "infrastructure_failure",
            "admission_rejected",
        ] {
            assert_eq!(
                serde_json::from_value::<ValidationTerminalStatus>(serde_json::json!(historical))
                    .expect("historical persisted failure status"),
                ValidationTerminalStatus::Failed
            );
        }
    }

    #[test]
    fn evidence_contract_validation_result_preserves_legacy_and_new_fields() {
        let mut wire = serde_json::json!({
            "proofKey": {
                "repository": "repo",
                "cwd": ".",
                "canonicalRouteHash": "route",
                "implementationIdentity": "implementation",
                "coverageIdentity": "coverage",
                "environmentIdentity": "environment",
                "toolchainIdentity": "toolchain",
                "configurationIdentity": "configuration",
                "validationContractVersion": 1
            },
            "route": {
                "leaves": [{
                    "argv": ["cargo", "test", "focused"],
                    "covered_paths": ["src/owner.rs"],
                    "covered_contracts": ["focused behavior"],
                    "timeout_ms": 300000
                }],
                "ordering": "stop_on_failure"
            },
            "callId": "call-1",
            "status": "failed",
            "durationMs": 42,
            "failureExcerpt": "failed",
            "freshness": "executed"
        });
        let legacy: ValidationResult =
            serde_json::from_value(wire.clone()).expect("legacy validation result");
        assert_eq!(legacy.failure_signature, None);
        assert_eq!(legacy.selected_test_count, None);

        wire["failureSignature"] = serde_json::json!("validation-failure-v1:exit-1:abc");
        wire["selectedTestCount"] = serde_json::json!(3);
        let current: ValidationResult =
            serde_json::from_value(wire).expect("current validation result");
        assert_eq!(
            current.failure_signature.as_deref(),
            Some("validation-failure-v1:exit-1:abc")
        );
        assert_eq!(current.selected_test_count, Some(3));
        let roundtrip = serde_json::to_value(current).expect("validation result roundtrip");
        assert_eq!(
            roundtrip["failureSignature"],
            "validation-failure-v1:exit-1:abc"
        );
        assert_eq!(roundtrip["selectedTestCount"], 3);
    }
}
