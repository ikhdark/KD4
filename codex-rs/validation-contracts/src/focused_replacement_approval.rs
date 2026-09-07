use crate::canonical::ContractError;
use crate::canonical::Sha256HexV1;
use crate::canonical::canonical_jcs_of;
use crate::canonical::parse_canonical_jcs;
use crate::canonical::proof_hash;
use crate::canonical::validate_identifier;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;
use uuid::Variant;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusedReplacementApprovalReceiptV1 {
    pub format_id: String,
    pub schema_version: u32,
    pub attempt_id: String,
    pub focused_validation_id: String,
    pub classification: String,
    pub frozen_inventory_hash: Sha256HexV1,
    pub focused_inventory_catalog_semantic_sha256: Sha256HexV1,
    pub inventory_discovery_processes_sha256: Sha256HexV1,
    pub policy_id: String,
    pub policy_runner_bundle_sha256: Sha256HexV1,
    pub workspace_fingerprint: Sha256HexV1,
    pub mutation_epoch: u64,
    pub receipt_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusedReplacementApprovalCurrentContextV1 {
    pub format_id: String,
    pub schema_version: u32,
    pub attempt_id: String,
    pub focused_validation_id: String,
    pub classification: String,
    pub frozen_inventory_hash: Sha256HexV1,
    pub focused_inventory_catalog_semantic_sha256: Sha256HexV1,
    pub inventory_discovery_processes_sha256: Sha256HexV1,
    pub policy_id: String,
    pub policy_runner_bundle_sha256: Sha256HexV1,
    pub workspace_fingerprint: Sha256HexV1,
    pub mutation_epoch: u64,
}

impl FocusedReplacementApprovalReceiptV1 {
    pub const FORMAT_ID: &'static str = "kd4.focused-replacement-approval-receipt.v1";
    pub const HASH_DOMAIN: &'static str = "kd4.focused-replacement-approval-receipt.v1";
    pub const FOCUSED_VALIDATION_ID: &'static str = "inventory.frozen-reconciliation";
    pub const TRANSITION_VALIDATION_ID: &'static str = "inventory.transition-readiness";
    pub const CLASSIFICATION: &'static str = "confirmed-pass";

    pub fn parse_canonical(bytes: &[u8]) -> Result<Self, ContractError> {
        let value = parse_canonical_jcs(bytes)?;
        let receipt: Self = serde_json::from_value(value)
            .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        canonical_jcs_of(self)?;
        if self.format_id != Self::FORMAT_ID
            || self.schema_version != 1
            || !matches!(
                self.focused_validation_id.as_str(),
                Self::FOCUSED_VALIDATION_ID | Self::TRANSITION_VALIDATION_ID
            )
            || self.classification != Self::CLASSIFICATION
            || self.mutation_epoch > 9_007_199_254_740_991
        {
            return Err(ContractError::InvalidContract(
                "invalid focused replacement approval receipt envelope".to_owned(),
            ));
        }
        validate_canonical_uuid_v7(&self.attempt_id)?;
        validate_identifier(&self.policy_id)?;
        if self.receipt_sha256()? != self.receipt_sha256 {
            return Err(ContractError::InvalidContract(
                "focused replacement approval receipt hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn receipt_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        let projection = FocusedReplacementApprovalReceiptHashProjectionV1 {
            format_id: &self.format_id,
            schema_version: self.schema_version,
            attempt_id: &self.attempt_id,
            focused_validation_id: &self.focused_validation_id,
            classification: &self.classification,
            frozen_inventory_hash: &self.frozen_inventory_hash,
            focused_inventory_catalog_semantic_sha256: &self
                .focused_inventory_catalog_semantic_sha256,
            inventory_discovery_processes_sha256: &self.inventory_discovery_processes_sha256,
            policy_id: &self.policy_id,
            policy_runner_bundle_sha256: &self.policy_runner_bundle_sha256,
            workspace_fingerprint: &self.workspace_fingerprint,
            mutation_epoch: self.mutation_epoch,
        };
        proof_hash(Self::HASH_DOMAIN, &projection)
    }

    pub fn validate_current_context(
        &self,
        trusted: &FocusedReplacementApprovalCurrentContextV1,
    ) -> Result<(), ContractError> {
        self.validate()?;
        if self.format_id != trusted.format_id
            || self.schema_version != trusted.schema_version
            || self.attempt_id != trusted.attempt_id
            || self.focused_validation_id != trusted.focused_validation_id
            || self.classification != trusted.classification
            || self.frozen_inventory_hash != trusted.frozen_inventory_hash
            || self.focused_inventory_catalog_semantic_sha256
                != trusted.focused_inventory_catalog_semantic_sha256
            || self.inventory_discovery_processes_sha256
                != trusted.inventory_discovery_processes_sha256
            || self.policy_id != trusted.policy_id
            || self.policy_runner_bundle_sha256 != trusted.policy_runner_bundle_sha256
            || self.workspace_fingerprint != trusted.workspace_fingerprint
            || self.mutation_epoch != trusted.mutation_epoch
        {
            return Err(ContractError::InvalidContract(
                "focused replacement approval receipt does not match trusted current context"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct FocusedReplacementApprovalReceiptHashProjectionV1<'a> {
    format_id: &'a str,
    schema_version: u32,
    attempt_id: &'a str,
    focused_validation_id: &'a str,
    classification: &'a str,
    frozen_inventory_hash: &'a Sha256HexV1,
    focused_inventory_catalog_semantic_sha256: &'a Sha256HexV1,
    inventory_discovery_processes_sha256: &'a Sha256HexV1,
    policy_id: &'a str,
    policy_runner_bundle_sha256: &'a Sha256HexV1,
    workspace_fingerprint: &'a Sha256HexV1,
    mutation_epoch: u64,
}

fn validate_canonical_uuid_v7(value: &str) -> Result<(), ContractError> {
    let parsed = Uuid::parse_str(value).map_err(|_| {
        ContractError::InvalidContract(
            "focused replacement approval attempt ID must be a canonical UUIDv7".to_owned(),
        )
    })?;
    if parsed.to_string() != value
        || parsed.get_version_num() != 7
        || parsed.get_variant() != Variant::RFC4122
    {
        return Err(ContractError::InvalidContract(
            "focused replacement approval attempt ID must be a canonical UUIDv7".to_owned(),
        ));
    }
    Ok(())
}
