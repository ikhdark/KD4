//! Closed wire contract for the structural historical-replacement proposal.
//!
//! The caller must obtain `approval` from a trusted runtime source before calling
//! [`HistoricalReplacementAcceptanceProposalV1::validate`]. Receipt self-hash
//! validation here establishes integrity, not authority or replay admission.

use crate::canonical::ContractError;
use crate::canonical::MustBeNullV1;
use crate::canonical::Sha256HexV1;
use crate::canonical::canonical_jcs_of;
use crate::canonical::parse_canonical_jcs;
use crate::canonical::proof_hash;
use crate::canonical::validate_nonempty_nfc;
use crate::focused_replacement_approval::FocusedReplacementApprovalReceiptV1;
use crate::inventory_v2::derive_frozen_v1_historical_replacement_graph_v1;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalReplacementScopeReviewV1 {
    pub review_scope_id: String,
    pub review_scope_sha256: Sha256HexV1,
    pub disposition: HistoricalReplacementScopeReviewDispositionV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoricalReplacementScopeReviewDispositionV1 {
    #[serde(rename = "reviewed-no-incorrect-behavior")]
    ReviewedNoIncorrectBehavior,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusedReplacementApprovalReceiptRefV1 {
    pub format_id: String,
    pub schema_version: u32,
    pub attempt_id: String,
    pub focused_validation_id: String,
    pub receipt_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalReplacementAcceptanceProposalV1 {
    pub format_id: String,
    pub schema_version: u32,
    pub frozen_graph_sha256: Sha256HexV1,
    pub review_plan_sha256: Sha256HexV1,
    pub baseline_count: u32,
    pub edge_count: u32,
    pub successor_count: u32,
    pub review_scope_count: u32,
    pub focused_replacement_approval_receipt_ref: FocusedReplacementApprovalReceiptRefV1,
    pub scope_reviews: Vec<HistoricalReplacementScopeReviewV1>,
    pub scope_review_set_sha256: Sha256HexV1,
    pub activation_authority: MustBeNullV1,
    pub proposal_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalReplacementEdgeV1 {
    baseline_id: String,
    replacement_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalReplacementReviewScopeComponentV1 {
    baseline_ids: Vec<String>,
    edges: Vec<HistoricalReplacementEdgeV1>,
    successor_ids: Vec<String>,
}

#[derive(Serialize)]
struct HistoricalReplacementReviewScopeHashProjectionV1<'a> {
    baseline_ids: &'a [String],
    edges: &'a [HistoricalReplacementEdgeV1],
    successor_ids: &'a [String],
}

#[derive(Serialize)]
struct HistoricalReplacementReviewScopeV1<'a> {
    baseline_ids: &'a [String],
    edges: &'a [HistoricalReplacementEdgeV1],
    successor_ids: &'a [String],
    review_scope_id: String,
    review_scope_sha256: Sha256HexV1,
}

#[derive(Serialize)]
struct HistoricalReplacementReviewPlanHashProjectionV1<'a> {
    format_id: &'static str,
    schema_version: u32,
    frozen_graph_sha256: &'a Sha256HexV1,
    baseline_count: u32,
    edge_count: u32,
    successor_count: u32,
    review_scope_count: u32,
    review_scopes: &'a [HistoricalReplacementReviewScopeV1<'a>],
}

#[derive(Serialize)]
struct HistoricalReplacementAcceptanceProposalHashProjectionV1<'a> {
    format_id: &'a str,
    schema_version: u32,
    frozen_graph_sha256: &'a Sha256HexV1,
    review_plan_sha256: &'a Sha256HexV1,
    baseline_count: u32,
    edge_count: u32,
    successor_count: u32,
    review_scope_count: u32,
    focused_replacement_approval_receipt_ref: &'a FocusedReplacementApprovalReceiptRefV1,
    scope_reviews: &'a [HistoricalReplacementScopeReviewV1],
    scope_review_set_sha256: &'a Sha256HexV1,
    activation_authority: MustBeNullV1,
}

impl HistoricalReplacementAcceptanceProposalV1 {
    pub const FORMAT_ID: &'static str = "kd4.historical-replacement-acceptance-proposal.v1";
    pub const FROZEN_GRAPH_HASH_DOMAIN: &'static str =
        "kd4.frozen-v1-historical-replacement-graph.v1";
    pub const FROZEN_GRAPH_SHA256: &'static str =
        crate::inventory_v2::FROZEN_V1_HISTORICAL_REPLACEMENT_GRAPH_SHA256;
    pub const REVIEW_SCOPE_HASH_DOMAIN: &'static str = "kd4.historical-replacement-review-scope.v1";
    pub const REVIEW_PLAN_HASH_DOMAIN: &'static str = "kd4.historical-replacement-review-plan.v1";
    pub const SCOPE_REVIEW_SET_HASH_DOMAIN: &'static str =
        "kd4.historical-replacement-scope-review-set.v1";
    pub const BASELINE_COUNT: u32 =
        crate::inventory_v2::FROZEN_V1_HISTORICAL_REPLACEMENT_BASELINE_COUNT as u32;
    pub const EDGE_COUNT: u32 =
        crate::inventory_v2::FROZEN_V1_HISTORICAL_REPLACEMENT_EDGE_COUNT as u32;
    pub const SUCCESSOR_COUNT: u32 =
        crate::inventory_v2::FROZEN_V1_HISTORICAL_REPLACEMENT_SUCCESSOR_COUNT as u32;
    pub const REVIEW_SCOPE_COUNT: u32 =
        crate::inventory_v2::FROZEN_V1_HISTORICAL_REPLACEMENT_COMPONENT_COUNT as u32;

    pub fn parse_canonical(
        bytes: &[u8],
        predecessor_ledger: &serde_json::Value,
        approval: &FocusedReplacementApprovalReceiptV1,
    ) -> Result<Self, ContractError> {
        let value = parse_canonical_jcs(bytes)?;
        let proposal: Self = serde_json::from_value(value)
            .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
        proposal.validate(predecessor_ledger, approval)?;
        Ok(proposal)
    }

    pub fn validate(
        &self,
        predecessor_ledger: &serde_json::Value,
        approval: &FocusedReplacementApprovalReceiptV1,
    ) -> Result<(), ContractError> {
        canonical_jcs_of(self)?;
        approval.validate()?;
        if self.format_id != Self::FORMAT_ID
            || self.schema_version != 1
            || self.baseline_count != Self::BASELINE_COUNT
            || self.edge_count != Self::EDGE_COUNT
            || self.successor_count != Self::SUCCESSOR_COUNT
            || self.review_scope_count != Self::REVIEW_SCOPE_COUNT
            || self.scope_reviews.len() != Self::REVIEW_SCOPE_COUNT as usize
            || self.frozen_graph_sha256.as_str() != Self::FROZEN_GRAPH_SHA256
        {
            return Err(ContractError::InvalidContract(
                "invalid historical replacement acceptance proposal envelope".to_owned(),
            ));
        }

        let graph = derive_frozen_v1_historical_replacement_graph_v1(predecessor_ledger)?;
        let graph_hash = proof_hash(Self::FROZEN_GRAPH_HASH_DOMAIN, &graph.projection)?;
        if graph_hash != self.frozen_graph_sha256 {
            return Err(ContractError::InvalidContract(
                "historical replacement acceptance proposal graph mismatch".to_owned(),
            ));
        }
        let components = graph.projection["components"]
            .as_array()
            .ok_or_else(|| {
                ContractError::InvalidContract(
                    "historical replacement graph has no component array".to_owned(),
                )
            })?
            .iter()
            .cloned()
            .map(|value| {
                serde_json::from_value::<HistoricalReplacementReviewScopeComponentV1>(value)
                    .map_err(|error| ContractError::InvalidJson(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if components.len() != Self::REVIEW_SCOPE_COUNT as usize {
            return Err(ContractError::InvalidContract(
                "historical replacement review plan scope count mismatch".to_owned(),
            ));
        }

        let mut review_scopes = Vec::with_capacity(components.len());
        let mut expected_reviews = Vec::with_capacity(components.len());
        for component in &components {
            let hash_projection = HistoricalReplacementReviewScopeHashProjectionV1 {
                baseline_ids: &component.baseline_ids,
                edges: &component.edges,
                successor_ids: &component.successor_ids,
            };
            let review_scope_sha256 = proof_hash(Self::REVIEW_SCOPE_HASH_DOMAIN, &hash_projection)?;
            let review_scope_id = format!("historical-replacement-review-v1.{review_scope_sha256}");
            review_scopes.push(HistoricalReplacementReviewScopeV1 {
                baseline_ids: &component.baseline_ids,
                edges: &component.edges,
                successor_ids: &component.successor_ids,
                review_scope_id: review_scope_id.clone(),
                review_scope_sha256: review_scope_sha256.clone(),
            });
            expected_reviews.push(HistoricalReplacementScopeReviewV1 {
                review_scope_id,
                review_scope_sha256,
                disposition:
                    HistoricalReplacementScopeReviewDispositionV1::ReviewedNoIncorrectBehavior,
            });
        }

        let plan_projection = HistoricalReplacementReviewPlanHashProjectionV1 {
            format_id: "kd4.historical-replacement-review-plan.v1",
            schema_version: 1,
            frozen_graph_sha256: &graph_hash,
            baseline_count: Self::BASELINE_COUNT,
            edge_count: Self::EDGE_COUNT,
            successor_count: Self::SUCCESSOR_COUNT,
            review_scope_count: Self::REVIEW_SCOPE_COUNT,
            review_scopes: &review_scopes,
        };
        if proof_hash(Self::REVIEW_PLAN_HASH_DOMAIN, &plan_projection)? != self.review_plan_sha256 {
            return Err(ContractError::InvalidContract(
                "historical replacement review plan hash mismatch".to_owned(),
            ));
        }

        expected_reviews.sort_by(|left, right| left.review_scope_id.cmp(&right.review_scope_id));
        for review in &self.scope_reviews {
            validate_nonempty_nfc(&review.review_scope_id, "historical review scope ID")?;
            if review.review_scope_id
                != format!(
                    "historical-replacement-review-v1.{}",
                    review.review_scope_sha256
                )
            {
                return Err(ContractError::InvalidContract(
                    "historical review scope ID does not match its scope hash".to_owned(),
                ));
            }
        }
        if self
            .scope_reviews
            .windows(2)
            .any(|pair| pair[0].review_scope_id >= pair[1].review_scope_id)
            || self.scope_reviews != expected_reviews
        {
            return Err(ContractError::InvalidContract(
                "scope reviews do not exactly close over the historical review plan".to_owned(),
            ));
        }
        if proof_hash(Self::SCOPE_REVIEW_SET_HASH_DOMAIN, &self.scope_reviews)?
            != self.scope_review_set_sha256
        {
            return Err(ContractError::InvalidContract(
                "historical scope review set hash mismatch".to_owned(),
            ));
        }

        let expected_receipt_ref = FocusedReplacementApprovalReceiptRefV1 {
            format_id: FocusedReplacementApprovalReceiptV1::FORMAT_ID.to_owned(),
            schema_version: 1,
            attempt_id: approval.attempt_id.clone(),
            focused_validation_id: FocusedReplacementApprovalReceiptV1::FOCUSED_VALIDATION_ID
                .to_owned(),
            receipt_sha256: approval.receipt_sha256.clone(),
        };
        if self.focused_replacement_approval_receipt_ref != expected_receipt_ref {
            return Err(ContractError::InvalidContract(
                "historical acceptance proposal focused approval receipt mismatch".to_owned(),
            ));
        }
        if self.proposal_sha256()? != self.proposal_sha256 {
            return Err(ContractError::InvalidContract(
                "historical replacement acceptance proposal hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn proposal_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        let projection = HistoricalReplacementAcceptanceProposalHashProjectionV1 {
            format_id: &self.format_id,
            schema_version: self.schema_version,
            frozen_graph_sha256: &self.frozen_graph_sha256,
            review_plan_sha256: &self.review_plan_sha256,
            baseline_count: self.baseline_count,
            edge_count: self.edge_count,
            successor_count: self.successor_count,
            review_scope_count: self.review_scope_count,
            focused_replacement_approval_receipt_ref: &self
                .focused_replacement_approval_receipt_ref,
            scope_reviews: &self.scope_reviews,
            scope_review_set_sha256: &self.scope_review_set_sha256,
            activation_authority: self.activation_authority,
        };
        proof_hash(Self::FORMAT_ID, &projection)
    }
}
