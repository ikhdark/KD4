//! Public protocol types for authenticated Desktop activation evidence.

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopPublishInstallEvidenceV1 {
    pub schema_version: u32,
    pub trusted_producer_version: u32,
    pub publisher_evidence_id: String,
    pub thread_id: String,
    pub evidence_epoch: u64,
    #[serde(rename = "implementationIdentity")]
    pub implementation_identity_hash: String,
    pub activation_obligation_identity: String,
    #[serde(rename = "publishId")]
    pub publish_identity: String,
    #[serde(default)]
    pub install_generation: u64,
    pub expected_installed_executable_path: String,
    #[serde(rename = "installedFileSha256")]
    pub installed_executable_sha256: String,
    #[serde(rename = "installationTimestamp")]
    pub issued_at: String,
    #[serde(default)]
    pub expires_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopActivationVerificationError {
    NoAuthenticatedHostTransport,
    InvalidAuthoritativeEvidence,
    AuthoritativeEvidenceStale,
    ImplementationIdentityMismatch,
    RunningExecutableMismatch,
    RunningProcessIdentityMissing,
    ChallengeMissingOrConsumed,
    ChallengeExpired,
    ChallengeIdentityMismatch,
    AuthenticatedChannelMismatch,
    InitializedProcessMismatch,
    InvalidDesktopObservation,
    ActivationObligationChanged,
    ChallengeAlreadyRecordedWithDifferentPayload,
    PersistenceFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopActivationObligation {
    pub thread_id: String,
    pub evidence_epoch: u64,
    pub implementation_identity: String,
    pub activation_obligation_identity: String,
    pub requiring_plan_step_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopActivationChallenge {
    pub challenge_id: String,
    pub thread_id: String,
    pub evidence_epoch: u64,
    pub implementation_identity: String,
    pub activation_obligation_identity: String,
    pub publisher_evidence_id: String,
    pub expected_installed_executable_path: String,
    pub expected_installed_executable_sha256: String,
    pub publish_id: String,
    pub issued_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopActivationRecordObservation {
    pub challenge_id: String,
    pub desktop_process_id: u32,
    pub desktop_executable_path: String,
    pub observation_timestamp: String,
    pub initialization_observation_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopActivationRecordResult {
    pub challenge_id: String,
    pub recorded_at: String,
    pub already_recorded: bool,
}
