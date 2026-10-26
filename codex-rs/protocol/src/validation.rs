use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Authoritative runtime policy for validation command execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum ValidationMode {
    Off,
    Compile,
    #[default]
    #[serde(rename = "full.5", alias = "benchmark")]
    #[ts(rename = "full.5")]
    FullHalf,
    Full,
}

/// Maximum admitted Full .5/FULL runtime/tooling validation attempts in one root turn, including its children; excludes targeted compilation.
pub const DEFAULT_VALIDATION_FULL_BUDGET: usize = 2;

impl ValidationMode {
    /// Parse an exact command on the first line of trusted, top-level user input.
    /// A task may follow on subsequent lines. Arbitrary instruction or tool text
    /// must never reach this API.
    pub fn from_user_command(text: &str) -> Option<Self> {
        let first_line = text.split('\n').next()?;
        let first_line = first_line.strip_suffix('\r').unwrap_or(first_line);
        match first_line {
            "/validation off" => Some(Self::Off),
            "/validation compile" => Some(Self::Compile),
            "/validation full.5" | "/validation full .5" | "/validation benchmark" => Some(Self::FullHalf),
            "/validation full" => Some(Self::Full),
            _ => None,
        }
    }
}

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
        validate_covered_paths(&raw.covered_paths).map_err(serde::de::Error::custom)?;
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
}

/// The settled result of one validation command execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    if argv.first().is_none_or(|value| value.trim().is_empty()) {
        return Err("validation argv must contain a non-empty executable".to_string());
    }
    validate_covered_paths(covered_paths)
}

fn validate_covered_paths(covered_paths: &[String]) -> Result<(), String> {
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
        || path.contains('\0')
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

