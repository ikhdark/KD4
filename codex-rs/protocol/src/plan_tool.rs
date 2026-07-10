use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

// Types for the TODO tool arguments matching codex-vscode/todo-mcp/src/main.rs
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct PlanItemArg {
    pub step: String,
    pub status: StepStatus,
}

#[derive(Debug, Clone, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct UpdatePlanArgs {
    /// Arguments for the `update_plan` todo/checklist tool (not plan mode).
    #[serde(default)]
    pub explanation: Option<String>,
    pub plan: Vec<PlanItemArg>,
}

impl<'de> Deserialize<'de> for PlanItemArg {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawPlanItemArg {
            step: String,
            status: StepStatus,
        }

        let raw = RawPlanItemArg::deserialize(deserializer)?;
        validate_plan_step(&raw.step).map_err(serde::de::Error::custom)?;

        Ok(Self {
            step: raw.step,
            status: raw.status,
        })
    }
}

impl<'de> Deserialize<'de> for UpdatePlanArgs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawUpdatePlanArgs {
            #[serde(default)]
            explanation: Option<String>,
            plan: Vec<PlanItemArg>,
        }

        let raw = RawUpdatePlanArgs::deserialize(deserializer)?;
        validate_plan_items(&raw.plan).map_err(serde::de::Error::custom)?;

        Ok(Self {
            explanation: raw.explanation,
            plan: raw.plan,
        })
    }
}

fn validate_plan_step(step: &str) -> Result<(), &'static str> {
    if step.trim().is_empty() {
        Err("plan step cannot be empty")
    } else {
        Ok(())
    }
}

fn validate_plan_items(plan: &[PlanItemArg]) -> Result<(), &'static str> {
    let in_progress_count = plan
        .iter()
        .filter(|item| item.status == StepStatus::InProgress)
        .count();
    if in_progress_count > 1 {
        Err("plan can contain at most one in_progress item")
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "plan_tool_tests.rs"]
mod tests;
