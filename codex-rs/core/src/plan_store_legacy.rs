//! Read-only d49 lineage wire representation. This authenticates historical
//! revisions before projection; it does not restore the retired task engine.

use super::{PlanLineage, PlanRequirement, UpdatePlanArgs};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Lineage {
    #[serde(default)]
    requirements: BTreeMap<String, PlanRequirement>,
    #[serde(default)]
    step_requirements: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    step_identities: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workflow: BTreeMap<String, ExecutionNode>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    resolved_requirements: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    step_revisions: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    accepted_supersessions: BTreeMap<String, InputReference>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    acceptance: BTreeMap<String, Acceptance>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    acceptance_history: BTreeMap<String, Acceptance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    acceptance_reviewed_revision: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExecutionNode {
    assignment_id: String,
    dependencies: Vec<String>,
    capability_profile: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InputReference {
    turn_id: String,
    sha256: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Acceptance {
    text: String,
    origin: InputReference,
    requirement: Requirement,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    plan_requirement_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blocker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retired_by: Option<InputReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retirement_reason: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Requirement {
    Inspection { environment_id: String, path: String, start: u64, end: u64 },
    Validation {
        environment_id: String, path: String, test_execution: bool,
        #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
        required_tests: BTreeSet<TestIdentity>,
    },
    Assignment { assignment_id: String, attempt_id: String },
}

#[derive(Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct TestIdentity {
    binary: String,
    helpers: Vec<String>,
    test: String,
}

pub(super) fn revision(plan: &UpdatePlanArgs, lineage: &PlanLineage) -> Option<String> {
    // Struct field order is part of the old hash contract. JSON Value alone
    // would alphabetize nested fields and reject an authentic saved revision.
    let historical: Lineage = serde_json::from_value(serde_json::to_value(lineage).ok()?).ok()?;
    let bytes = serde_json::to_vec(&(Some(plan), historical)).ok()?;
    Some(hex::encode(Sha256::digest(bytes)))
}
