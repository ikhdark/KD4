use codex_protocol::config_types::CollaborationModeMask as CoreCollaborationModeMask;
use codex_protocol::config_types::ModeKind;
use codex_protocol::openai_models::ReasoningEffort;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// EXPERIMENTAL - list collaboration mode presets.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CollaborationModeListParams {}

/// EXPERIMENTAL - collaboration mode preset metadata for clients.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CollaborationModeMask {
    pub name: String,
    pub mode: Option<ModeKind>,
    pub model: Option<String>,
    #[serde(
        rename = "reasoning_effort",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::protocol::serde_helpers::deserialize_double_option",
        serialize_with = "crate::protocol::serde_helpers::serialize_double_option"
    )]
    #[ts(rename = "reasoning_effort")]
    #[ts(optional = nullable)]
    pub reasoning_effort: Option<Option<ReasoningEffort>>,
}

impl From<CoreCollaborationModeMask> for CollaborationModeMask {
    fn from(value: CoreCollaborationModeMask) -> Self {
        Self {
            name: value.name,
            mode: value.mode,
            model: value.model,
            reasoning_effort: value.reasoning_effort,
        }
    }
}

/// EXPERIMENTAL - collaboration mode presets response.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CollaborationModeListResponse {
    pub data: Vec<CollaborationModeMask>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reasoning_effort_preserves_absent_null_and_value() {
        for (effort, wire) in [
            (None, json!({"name": "test", "mode": null, "model": null})),
            (
                Some(None),
                json!({"name": "test", "mode": null, "model": null, "reasoning_effort": null}),
            ),
            (
                Some(Some(ReasoningEffort::High)),
                json!({"name": "test", "mode": null, "model": null, "reasoning_effort": "high"}),
            ),
        ] {
            let mask = CollaborationModeMask {
                name: "test".into(),
                mode: None,
                model: None,
                reasoning_effort: effort,
            };
            assert_eq!(serde_json::to_value(&mask).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<CollaborationModeMask>(wire).unwrap(),
                mask
            );
        }
    }
}
