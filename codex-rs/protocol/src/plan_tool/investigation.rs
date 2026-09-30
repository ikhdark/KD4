use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use ts_rs::TS;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Investigating,
    Implementing,
    Blocked,
    Resolved,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisStatus {
    Open,
    RuledOut,
    Established,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Hypothesis {
    pub id: String,
    pub explanation: String,
    pub status: HypothesisStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticAction {
    pub tool: String,
    /// Exact function arguments, or the raw string for a freeform tool.
    pub input: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub id: String,
    pub question: String,
    pub hypothesis_ids: Vec<String>,
    pub expected_outcomes: Vec<String>,
    pub obtainable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic_action: Option<DiagnosticAction>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    UncertaintyResolved,
    HypothesisEliminated,
    DecisionChanged,
    FailureReproduced,
    CauseEstablished,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub kind: FindingKind,
    pub observation_id: String,
    pub hypothesis_ids: Vec<String>,
    pub uncertainty: String,
    pub evidence: String,
    pub conclusion: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Investigation {
    pub symptom: String,
    pub phase: Phase,
    pub hypotheses: Vec<Hypothesis>,
    pub unknowns: Vec<String>,
    pub next_observation: Observation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding: Option<Finding>,
}
