use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use ts_rs::TS;

// Types for the TODO tool arguments matching codex-vscode/todo-mcp/src/main.rs
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    #[default]
    Pending,
    InProgress,
    Implemented,
    Passed,
    Blocked,
    Skipped,
    /// Legacy success state retained for older clients. The task-evidence
    /// ledger treats this as an implementation claim until fresh proof exists.
    Completed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct PlanItemArg {
    /// Stable caller-supplied identifier. Older callers may omit it; the live
    /// task-evidence ledger derives and returns a deterministic compatibility id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub id: Option<String>,
    pub step: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_criteria: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated_artifacts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risks: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub requires_desktop_activation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, TS)]
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
            #[serde(default)]
            id: Option<String>,
            step: String,
            status: StepStatus,
            #[serde(default)]
            depends_on: Vec<String>,
            #[serde(default)]
            acceptance_criteria: Vec<String>,
            #[serde(default)]
            runtime_paths: Vec<String>,
            #[serde(default)]
            generated_artifacts: Vec<String>,
            #[serde(default)]
            risks: Vec<String>,
            #[serde(default)]
            requires_desktop_activation: bool,
        }

        let raw = RawPlanItemArg::deserialize(deserializer)?;
        validate_plan_step(&raw.step).map_err(serde::de::Error::custom)?;
        validate_optional_id(raw.id.as_deref()).map_err(serde::de::Error::custom)?;
        validate_nonblank_values("dependency id", &raw.depends_on)
            .map_err(serde::de::Error::custom)?;
        validate_nonblank_values("acceptance criterion", &raw.acceptance_criteria)
            .map_err(serde::de::Error::custom)?;
        validate_nonblank_values("runtime path", &raw.runtime_paths)
            .map_err(serde::de::Error::custom)?;
        validate_nonblank_values("generated artifact", &raw.generated_artifacts)
            .map_err(serde::de::Error::custom)?;
        validate_nonblank_values("risk", &raw.risks).map_err(serde::de::Error::custom)?;

        Ok(Self {
            id: raw.id,
            step: raw.step,
            status: raw.status,
            depends_on: raw.depends_on,
            acceptance_criteria: raw.acceptance_criteria,
            runtime_paths: raw.runtime_paths,
            generated_artifacts: raw.generated_artifacts,
            risks: raw.risks,
            requires_desktop_activation: raw.requires_desktop_activation,
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

fn validate_optional_id(id: Option<&str>) -> Result<(), &'static str> {
    let Some(id) = id else {
        return Ok(());
    };
    if id.trim().is_empty() {
        return Err("plan step id cannot be empty");
    }
    if id.len() > 96
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err("plan step id must use at most 96 ASCII letters, digits, '.', '_', or '-'");
    }
    Ok(())
}

fn validate_nonblank_values(label: &'static str, values: &[String]) -> Result<(), String> {
    if values.iter().any(|value| value.trim().is_empty()) {
        Err(format!("plan {label} cannot be empty"))
    } else {
        Ok(())
    }
}

fn validate_plan_items(plan: &[PlanItemArg]) -> Result<(), String> {
    let in_progress_count = plan
        .iter()
        .filter(|item| item.status == StepStatus::InProgress)
        .count();
    if in_progress_count > 1 {
        return Err("plan can contain at most one in_progress item".to_string());
    }

    let mut explicit_ids = BTreeMap::new();
    for item in plan {
        if let Some(id) = item.id.as_deref()
            && explicit_ids.insert(id, item).is_some()
        {
            return Err(format!("plan step id `{id}` is duplicated"));
        }
        let mut dependencies = BTreeSet::new();
        for dependency in &item.depends_on {
            if !dependencies.insert(dependency.as_str()) {
                return Err(format!(
                    "plan step `{}` repeats dependency `{dependency}`",
                    item.step
                ));
            }
            if item.id.as_deref() == Some(dependency.as_str()) {
                return Err(format!("plan step `{dependency}` cannot depend on itself"));
            }
        }
    }

    for item in plan {
        for dependency in &item.depends_on {
            if !explicit_ids.contains_key(dependency.as_str()) {
                return Err(format!(
                    "plan dependency `{dependency}` does not name an explicit step id"
                ));
            }
        }
    }

    let mut visited = BTreeSet::new();
    let mut visiting = BTreeSet::new();
    for id in explicit_ids.keys().copied() {
        visit_dependency(id, &explicit_ids, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn visit_dependency<'a>(
    id: &'a str,
    items: &BTreeMap<&'a str, &'a PlanItemArg>,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> Result<(), String> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        return Err(format!("plan dependency cycle includes `{id}`"));
    }
    if let Some(item) = items.get(id) {
        for dependency in &item.depends_on {
            visit_dependency(dependency, items, visiting, visited)?;
        }
    }
    visiting.remove(id);
    visited.insert(id);
    Ok(())
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
#[path = "plan_tool_tests.rs"]
mod tests;
