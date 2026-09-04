use crate::canonical::ContractError;
use crate::canonical::Sha256HexV1;
use crate::canonical::proof_hash;
use crate::canonical::validate_nfc;
use crate::canonical::validate_nonempty_nfc;
use crate::selection::ExecutableIdentityV1;
use crate::selection::InventoryAuthorityRefV1;
use crate::selection::ValidationIdV1;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmedFailureReceiptV1 {
    pub attempt_id: String,
    pub classification: ConfirmedFailureClassificationV1,
    pub execution_ids: Vec<String>,
    pub focused_projection_sha256: Sha256HexV1,
    pub mutation_epoch: u64,
    pub workspace_fingerprint: Sha256HexV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfirmedFailureClassificationV1 {
    #[serde(rename = "confirmed-validation-failure")]
    ConfirmedValidationFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelevantMutationReceiptV1 {
    pub changed_input_leaf_ids: Vec<String>,
    pub classification: RelevantMutationClassificationV1,
    pub from_epoch: u64,
    pub input_contract_set_sha256: Sha256HexV1,
    pub production_delta_sha256: Sha256HexV1,
    pub to_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelevantMutationClassificationV1 {
    #[serde(rename = "relevant-non-test-product-runtime")]
    RelevantNonTestProductRuntime,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmedPassReceiptV1 {
    pub attempt_id: String,
    pub classification: ConfirmedPassClassificationV1,
    pub execution_ids: Vec<String>,
    pub focused_projection_sha256: Sha256HexV1,
    pub mutation_epoch: u64,
    pub workspace_fingerprint: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntendedExecutionProjectionV1 {
    pub attempt_id: String,
    pub executable_identities: Vec<ExecutableIdentityV1>,
    pub intended_count: u64,
    pub inventory_authority: InventoryAuthorityRefV1,
    pub selection_v1_sha256: Sha256HexV1,
    pub validation_id: ValidationIdV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationAttemptClassificationV1 {
    ConfirmedPass,
    ConfirmedValidationFailure,
    PreResultError,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutedOutcomeV1 {
    pub execution_id: String,
    pub identity: ExecutableIdentityV1,
    pub outcome: ExecutedOutcomeKindV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutedOutcomeKindV1 {
    Passed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationReceiptProjectionV1 {
    pub attempt_id: String,
    pub classification: ValidationAttemptClassificationV1,
    pub executed_count: u64,
    pub intended_execution_projection: IntendedExecutionProjectionV1,
    pub intended_execution_projection_sha256: Sha256HexV1,
    pub mismatch_codes: Vec<String>,
    pub outcomes: Vec<ExecutedOutcomeV1>,
    pub schema_version: u8,
    pub selected_count: u64,
    pub started_count: u64,
    pub terminal_count: u64,
    pub validation_id: ValidationIdV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfirmedPassClassificationV1 {
    #[serde(rename = "confirmed-pass")]
    ConfirmedPass,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedDefectReceiptV1 {
    pub baseline_ids: Vec<String>,
    pub baseline_obligation_ids: Vec<String>,
    pub defect_id: String,
    pub incorrect_behavior: String,
    pub failure: ConfirmedFailureReceiptV1,
    pub mutation: RelevantMutationReceiptV1,
    pub pass: ConfirmedPassReceiptV1,
    pub receipt_sha256: Sha256HexV1,
    pub replacement_edge_ids: Vec<String>,
    pub resolved_entry_set_sha256: Sha256HexV1,
    pub schema_version: u8,
    pub selection_v1_sha256: Sha256HexV1,
}

impl IntendedExecutionProjectionV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.intended-execution-projection.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        if self.attempt_id.is_empty()
            || self.executable_identities.is_empty()
            || self.intended_count != self.executable_identities.len() as u64
        {
            return Err(ContractError::InvalidContract(
                "intended execution projection requires an exact nonzero identity set".to_owned(),
            ));
        }
        validate_nfc(&self.attempt_id)?;
        if self
            .executable_identities
            .iter()
            .any(|identity| identity.validation_id() != &self.validation_id)
        {
            return Err(ContractError::InvalidContract(
                "intended identities must belong to the intended validation".to_owned(),
            ));
        }
        let identities = self
            .executable_identities
            .iter()
            .map(crate::canonical::canonical_jcs_of)
            .collect::<Result<Vec<_>, _>>()?;
        if identities.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ContractError::InvalidContract(
                "intended identities must be sorted and unique by canonical identity bytes"
                    .to_owned(),
            ));
        }
        for identity in &self.executable_identities {
            identity.validate()?;
        }
        Ok(())
    }

    pub fn semantic_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        self.validate()?;
        proof_hash(Self::HASH_DOMAIN, self)
    }
}

impl ValidationReceiptProjectionV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.validation-receipt-projection.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        if self.schema_version != 1 || self.attempt_id.is_empty() {
            return Err(ContractError::InvalidContract(
                "validation receipt requires schema version 1 and an attempt ID".to_owned(),
            ));
        }
        validate_nfc(&self.attempt_id)?;
        self.intended_execution_projection.validate()?;
        if self.attempt_id != self.intended_execution_projection.attempt_id
            || self.validation_id != self.intended_execution_projection.validation_id
            || proof_hash(
                IntendedExecutionProjectionV1::HASH_DOMAIN,
                &self.intended_execution_projection,
            )? != self.intended_execution_projection_sha256
        {
            return Err(ContractError::InvalidContract(
                "validation receipt does not bind its intended execution projection".to_owned(),
            ));
        }
        ensure_sorted_unique(&self.mismatch_codes, "validation mismatch codes")?;
        let outcome_identities = self
            .outcomes
            .iter()
            .map(|outcome| crate::canonical::canonical_jcs_of(&outcome.identity))
            .collect::<Result<Vec<_>, _>>()?;
        if outcome_identities.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ContractError::InvalidContract(
                "validation outcomes must be normalized to canonical intended-identity order"
                    .to_owned(),
            ));
        }
        let mut execution_ids = std::collections::BTreeSet::new();
        for outcome in &self.outcomes {
            outcome.identity.validate()?;
            if !self
                .intended_execution_projection
                .executable_identities
                .contains(&outcome.identity)
            {
                return Err(ContractError::InvalidContract(
                    "validation outcome is outside the intended identity set".to_owned(),
                ));
            }
            if outcome.execution_id.is_empty() {
                return Err(ContractError::InvalidContract(
                    "executed outcome requires an execution ID".to_owned(),
                ));
            }
            validate_nfc(&outcome.execution_id)?;
            if !execution_ids.insert(&outcome.execution_id) {
                return Err(ContractError::InvalidContract(
                    "validation outcome execution IDs must be unique".to_owned(),
                ));
            }
        }
        let intended_count = self.intended_execution_projection.intended_count;
        let observed_counts_are_consistent = self.selected_count <= intended_count
            && self.started_count <= self.selected_count
            && self.terminal_count <= self.started_count
            && self.terminal_count == self.executed_count
            && self.executed_count == self.outcomes.len() as u64;
        let complete = observed_counts_are_consistent
            && self.started_count == intended_count
            && self.terminal_count == intended_count;
        match self.classification {
            ValidationAttemptClassificationV1::ConfirmedPass
                if complete
                    && self.mismatch_codes.is_empty()
                    && self
                        .outcomes
                        .iter()
                        .all(|outcome| outcome.outcome == ExecutedOutcomeKindV1::Passed) => {}
            ValidationAttemptClassificationV1::ConfirmedValidationFailure
                if observed_counts_are_consistent
                    && self.selected_count == intended_count
                    && self.executed_count > 0
                    && self
                        .outcomes
                        .iter()
                        .any(|outcome| outcome.outcome == ExecutedOutcomeKindV1::Failed) => {}
            ValidationAttemptClassificationV1::PreResultError
                if observed_counts_are_consistent
                    && !complete
                    && self
                        .outcomes
                        .iter()
                        .all(|outcome| outcome.outcome == ExecutedOutcomeKindV1::Passed)
                    && !self.mismatch_codes.is_empty() => {}
            _ => {
                return Err(ContractError::InvalidContract(
                    "validation receipt classification/count/outcome mismatch".to_owned(),
                ));
            }
        }
        Ok(())
    }

    pub fn semantic_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        self.validate()?;
        proof_hash(Self::HASH_DOMAIN, self)
    }
}

impl TrustedDefectReceiptV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.trusted-defect-receipt.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        if self.schema_version != 1
            || self.baseline_ids.is_empty()
            || self.baseline_obligation_ids.is_empty()
            || self.defect_id.is_empty()
            || self.incorrect_behavior.is_empty()
            || self.replacement_edge_ids.is_empty()
            || self.failure.execution_ids.is_empty()
            || self.pass.execution_ids.is_empty()
            || self.mutation.changed_input_leaf_ids.is_empty()
            || self.failure.mutation_epoch != self.mutation.from_epoch
            || self.pass.mutation_epoch != self.mutation.to_epoch
            || self.mutation.to_epoch <= self.mutation.from_epoch
        {
            return Err(ContractError::InvalidContract(
                "trusted defect receipt does not prove failure, relevant mutation, and fresh pass"
                    .to_owned(),
            ));
        }
        ensure_sorted_unique(&self.baseline_ids, "baseline IDs")?;
        ensure_sorted_unique(&self.baseline_obligation_ids, "baseline obligations")?;
        validate_nfc(&self.defect_id)?;
        validate_nfc(&self.incorrect_behavior)?;
        ensure_sorted_unique(&self.replacement_edge_ids, "replacement edges")?;
        ensure_sorted_unique(&self.failure.execution_ids, "failure execution IDs")?;
        ensure_sorted_unique(&self.pass.execution_ids, "pass execution IDs")?;
        ensure_sorted_unique(
            &self.mutation.changed_input_leaf_ids,
            "changed input leaf IDs",
        )?;
        validate_nonempty_nfc(&self.failure.attempt_id, "failure attempt ID")?;
        validate_nonempty_nfc(&self.pass.attempt_id, "pass attempt ID")?;
        if self.failure.workspace_fingerprint == self.pass.workspace_fingerprint {
            return Err(ContractError::InvalidContract(
                "trusted defect receipt requires a changed workspace fingerprint".to_owned(),
            ));
        }

        let expected = proof_hash(
            Self::HASH_DOMAIN,
            &TrustedDefectReceiptHashProjectionV1 {
                baseline_ids: &self.baseline_ids,
                baseline_obligation_ids: &self.baseline_obligation_ids,
                defect_id: &self.defect_id,
                incorrect_behavior: &self.incorrect_behavior,
                failure: &self.failure,
                mutation: &self.mutation,
                pass: &self.pass,
                replacement_edge_ids: &self.replacement_edge_ids,
                resolved_entry_set_sha256: &self.resolved_entry_set_sha256,
                schema_version: self.schema_version,
                selection_v1_sha256: &self.selection_v1_sha256,
            },
        )?;
        if self.receipt_sha256 != expected {
            return Err(ContractError::InvalidContract(
                "trusted defect receipt hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct TrustedDefectReceiptHashProjectionV1<'a> {
    baseline_ids: &'a [String],
    baseline_obligation_ids: &'a [String],
    defect_id: &'a str,
    incorrect_behavior: &'a str,
    failure: &'a ConfirmedFailureReceiptV1,
    mutation: &'a RelevantMutationReceiptV1,
    pass: &'a ConfirmedPassReceiptV1,
    replacement_edge_ids: &'a [String],
    resolved_entry_set_sha256: &'a Sha256HexV1,
    schema_version: u8,
    selection_v1_sha256: &'a Sha256HexV1,
}

fn ensure_sorted_unique(values: &[String], label: &str) -> Result<(), ContractError> {
    if values.iter().any(String::is_empty)
        || values.iter().any(|value| validate_nfc(value).is_err())
        || values.windows(2).any(|pair| pair[0] >= pair[1])
    {
        Err(ContractError::InvalidContract(format!(
            "{label} must be nonempty NFC strings sorted and unique"
        )))
    } else {
        Ok(())
    }
}
