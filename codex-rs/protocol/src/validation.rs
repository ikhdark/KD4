use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Repository-relative paths whose current contents are checked by a direct
/// validation command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ValidationCommandContext {
    pub covered_paths: Vec<String>,
}

impl<'de> Deserialize<'de> for ValidationCommandContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            covered_paths: Vec<String>,
        }

        let raw = Raw::deserialize(deserializer)?;
        if raw.covered_paths.is_empty()
            || raw
                .covered_paths
                .iter()
                .any(|value| value.trim().is_empty())
        {
            return Err(serde::de::Error::custom(
                "validation covered_paths must contain non-empty paths",
            ));
        }
        Ok(Self {
            covered_paths: raw.covered_paths,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum ValidationTerminalStatus {
    Succeeded,
    Unverified,
    Failed,
}

impl ValidationTerminalStatus {
    /// Whether this result is sufficient validation proof.
    pub fn is_success(self) -> bool {
        self == Self::Succeeded
    }

    /// Whether the validation command itself completed successfully.
    pub fn is_command_success(self) -> bool {
        matches!(self, Self::Succeeded | Self::Unverified)
    }
}

/// The settled result of one validation command execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ValidationResult {
    pub argv: Vec<String>,
    pub covered_paths: Vec<String>,
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
    pub raw_artifact_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub raw_artifact_sha256: Option<String>,
}

impl<'de> Deserialize<'de> for ValidationResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Raw {
            argv: Vec<String>,
            covered_paths: Vec<String>,
            call_id: String,
            #[serde(default)]
            process_id: Option<String>,
            status: ValidationTerminalStatus,
            duration_ms: u64,
            #[serde(default)]
            summary: Option<String>,
            #[serde(default)]
            failure_excerpt: Option<String>,
            #[serde(default)]
            raw_artifact_ref: Option<String>,
            #[serde(default)]
            raw_artifact_sha256: Option<String>,
        }

        let raw = Raw::deserialize(deserializer)?;
        validate_direct_validation_values(&raw.argv, &raw.covered_paths)
            .map_err(serde::de::Error::custom)?;
        if raw.call_id.trim().is_empty() {
            return Err(serde::de::Error::custom(
                "validation result call_id must be non-empty",
            ));
        }
        Ok(Self {
            argv: raw.argv,
            covered_paths: raw.covered_paths,
            call_id: raw.call_id,
            process_id: raw.process_id,
            status: raw.status,
            duration_ms: raw.duration_ms,
            summary: raw.summary,
            failure_excerpt: raw.failure_excerpt,
            raw_artifact_ref: raw.raw_artifact_ref,
            raw_artifact_sha256: raw.raw_artifact_sha256,
        })
    }
}

fn validate_direct_validation_values(
    argv: &[String],
    covered_paths: &[String],
) -> Result<(), String> {
    if argv.is_empty() || argv.iter().any(|value| value.trim().is_empty()) {
        return Err("validation argv values must be non-empty".to_string());
    }
    if covered_paths.is_empty() || covered_paths.iter().any(|value| value.trim().is_empty()) {
        return Err("validation covered_paths must contain non-empty paths".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    for path in covered_paths {
        if !is_normalized_repository_relative_scope(path) {
            return Err(format!(
                "validation covered path `{path}` must be a normalized repository-relative scope"
            ));
        }
        let identity = path.to_ascii_lowercase();
        if !seen.insert(identity) {
            return Err(format!(
                "validation covered path `{path}` must not be duplicated"
            ));
        }
    }
    Ok(())
}

fn is_normalized_repository_relative_scope(path: &str) -> bool {
    if path == "." {
        return true;
    }
    if path.is_empty()
        || path.trim() != path
        || path.contains('\\')
        || path.starts_with('/')
        || path.starts_with('~')
        || path.ends_with('/')
        || path.contains("//")
        || path.as_bytes().get(1) == Some(&b':')
    {
        return false;
    }
    path.split('/')
        .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

#[cfg(test)]
mod tests {
    use super::ValidationCommandContext;
    use super::ValidationResult;
    use super::ValidationTerminalStatus;
    use serde_json::json;

    #[test]
    fn terminal_status_wire_contract_is_lean_and_exact() {
        let statuses = [
            ValidationTerminalStatus::Succeeded,
            ValidationTerminalStatus::Unverified,
            ValidationTerminalStatus::Failed,
        ];
        assert_eq!(
            statuses
                .into_iter()
                .map(|status| serde_json::to_value(status).expect("status serialization"))
                .collect::<Vec<_>>(),
            vec![
                serde_json::json!("succeeded"),
                serde_json::json!("unverified"),
                serde_json::json!("failed"),
            ]
        );
        assert!(ValidationTerminalStatus::Succeeded.is_success());
        assert!(!ValidationTerminalStatus::Unverified.is_success());
        assert!(ValidationTerminalStatus::Unverified.is_command_success());
        assert!(serde_json::from_value::<ValidationTerminalStatus>(json!("superseded")).is_err());
        assert!(serde_json::from_value::<ValidationTerminalStatus>(json!("timed_out")).is_err());
    }

    #[test]
    fn lean_context_and_result_round_trip_and_reject_legacy_fields() {
        let context_value = json!({"covered_paths": ["codex-rs/core/src"]});
        let context = serde_json::from_value::<ValidationCommandContext>(context_value.clone())
            .expect("lean context");
        assert_eq!(
            serde_json::to_value(context).expect("context value"),
            context_value
        );

        let result_value = json!({
            "argv": ["cargo", "test", "-p", "codex-core", "focused_case"],
            "coveredPaths": ["codex-rs/core/src"],
            "callId": "call-1",
            "status": "succeeded",
            "durationMs": 12
        });
        let result =
            serde_json::from_value::<ValidationResult>(result_value.clone()).expect("lean result");
        assert_eq!(
            serde_json::to_value(result).expect("result value"),
            result_value
        );

        for legacy in [
            json!({"covered_paths": ["src"], "uncertainty": "legacy"}),
            json!({"covered_paths": ["src"], "covered_contracts": ["legacy"]}),
        ] {
            assert!(serde_json::from_value::<ValidationCommandContext>(legacy).is_err());
        }
    }

    #[test]
    fn lean_context_and_result_require_nonempty_paths_and_argv() {
        for value in [
            json!({"covered_paths": []}),
            json!({"covered_paths": [" "]}),
        ] {
            assert!(serde_json::from_value::<ValidationCommandContext>(value).is_err());
        }
        for value in [
            json!({
                "argv": [""], "coveredPaths": ["src"], "callId": "call-1",
                "status": "failed", "durationMs": 1
            }),
            json!({
                "argv": ["cargo"], "coveredPaths": [], "callId": "call-1",
                "status": "failed", "durationMs": 1
            }),
        ] {
            assert!(serde_json::from_value::<ValidationResult>(value).is_err());
        }
    }

    #[test]
    fn validation_result_rejects_non_normalized_or_duplicate_paths() {
        for covered_paths in [
            json!(["../src"]),
            json!(["/src"]),
            json!(["C:/src"]),
            json!(["src//lib"]),
            json!(["src/./lib"]),
            json!(["src", "src"]),
            json!(["src", "SRC"]),
        ] {
            assert!(
                serde_json::from_value::<ValidationResult>(json!({
                    "argv": ["cargo", "test"],
                    "coveredPaths": covered_paths,
                    "callId": "call-1",
                    "status": "succeeded",
                    "durationMs": 1
                }))
                .is_err()
            );
        }
    }
}
