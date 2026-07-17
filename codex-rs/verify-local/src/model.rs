use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeStruct;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

pub const VERIFY_LOCAL_JSON_PRODUCER: &str = "kd4.verify_local";
pub const VERIFY_LOCAL_V1_SCHEMA_VERSION: u64 = 1;
pub const VERIFY_LOCAL_V2_SCHEMA_VERSION: u64 = 2;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RawPath {
    bytes: Vec<u8>,
}

impl RawPath {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    pub fn from_utf8(path: impl Into<String>) -> Self {
        Self::new(path.into().into_bytes())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn as_utf8(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }

    pub fn bytes_base64(&self) -> String {
        BASE64_STANDARD.encode(&self.bytes)
    }

    pub fn display_lossy(&self) -> String {
        self.as_utf8()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("<raw:{}>", self.bytes_base64()))
    }

    pub fn validate_repository_relative(&self) -> Result<(), String> {
        if self.bytes.is_empty() {
            return Err("repository path is empty".to_string());
        }
        if self.bytes.contains(&0) {
            return Err("repository path contains NUL".to_string());
        }
        if self.bytes.starts_with(b"/")
            || self.bytes.starts_with(b"\\")
            || self.bytes.get(1) == Some(&b':')
        {
            return Err("repository path must be relative".to_string());
        }
        if self
            .bytes
            .split(|byte| *byte == b'/')
            .any(|part| part == b".." || part.is_empty())
        {
            return Err("repository path contains an invalid component".to_string());
        }
        #[cfg(windows)]
        if self.as_utf8().is_none() {
            return Err("non-UTF-8 paths are inconclusive on Windows".to_string());
        }
        Ok(())
    }

    #[cfg(unix)]
    pub fn to_os_string(&self) -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(self.bytes.clone())
    }

    #[cfg(windows)]
    pub fn to_os_string(&self) -> Result<OsString, String> {
        self.as_utf8()
            .map(OsString::from)
            .ok_or_else(|| "non-UTF-8 paths are inconclusive on Windows".to_string())
    }

    #[cfg(unix)]
    pub fn from_path(path: &Path) -> Self {
        use std::os::unix::ffi::OsStrExt;
        Self::new(path.as_os_str().as_bytes())
    }

    #[cfg(windows)]
    pub fn from_path(path: &Path) -> Result<Self, String> {
        path.to_str()
            .map(Self::from_utf8)
            .ok_or_else(|| "Windows path is not valid Unicode".to_string())
    }
}

impl Serialize for RawPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("RawPath", 2)?;
        state.serialize_field("utf8", &self.as_utf8())?;
        state.serialize_field("bytes_base64", &self.bytes_base64())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for RawPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireRawPath {
            utf8: Option<String>,
            bytes_base64: String,
        }

        let wire = WireRawPath::deserialize(deserializer)?;
        let bytes = BASE64_STANDARD
            .decode(wire.bytes_base64)
            .map_err(serde::de::Error::custom)?;
        if let Some(utf8) = wire.utf8
            && utf8.as_bytes() != bytes
        {
            return Err(serde::de::Error::custom(
                "RawPath utf8 and bytes_base64 do not describe the same bytes",
            ));
        }
        Ok(Self::new(bytes))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanMode {
    Plan,
    Fast,
    Final,
}

impl PlanMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Fast => "fast",
            Self::Final => "final",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Verdict {
    #[serde(rename = "PLANNED")]
    Planned,
    #[serde(rename = "VERIFIED")]
    Verified,
    #[serde(rename = "VERIFIED (no proof needed)")]
    VerifiedNoProof,
    #[serde(rename = "FAILED")]
    Failed,
    #[serde(rename = "INCONCLUSIVE")]
    Inconclusive,
    #[serde(rename = "NEEDS_SCOPE")]
    NeedsScope,
    #[serde(rename = "TOOLING_ERROR")]
    ToolingError,
    #[serde(rename = "NEEDS_REGEN")]
    NeedsRegen,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "PLANNED",
            Self::Verified => "VERIFIED",
            Self::VerifiedNoProof => "VERIFIED (no proof needed)",
            Self::Failed => "FAILED",
            Self::Inconclusive => "INCONCLUSIVE",
            Self::NeedsScope => "NEEDS_SCOPE",
            Self::ToolingError => "TOOLING_ERROR",
            Self::NeedsRegen => "NEEDS_REGEN",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            Self::Planned | Self::Verified | Self::VerifiedNoProof => 0,
            Self::Failed => 1,
            Self::Inconclusive => 2,
            Self::NeedsScope => 3,
            Self::ToolingError => 4,
            Self::NeedsRegen => 5,
        }
    }

    pub fn is_proof_bearing(self) -> bool {
        self == Self::Verified
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DirtyGroup {
    pub id: String,
    pub paths: Vec<RawPath>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScopeV2 {
    pub scope_id: String,
    pub source: String,
    pub active_files: Vec<RawPath>,
    pub owned_packages: Vec<String>,
    pub ignored_dirty_files: Vec<RawPath>,
    pub adjacent_packages: Vec<String>,
    pub stale_reasons: Vec<String>,
    pub dirty_groups: Vec<DirtyGroup>,
    pub surface_rules: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandArgV2 {
    Text { value: String },
    Path { path: RawPath },
}

impl CommandArgV2 {
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text {
            value: value.into(),
        }
    }

    pub fn path(path: RawPath) -> Self {
        Self::Path { path }
    }

    pub fn legacy_text(&self) -> Option<&str> {
        match self {
            Self::Text { value } => Some(value),
            Self::Path { path } => path.as_utf8(),
        }
    }

    pub fn display_lossy(&self) -> String {
        match self {
            Self::Text { value } => value.clone(),
            Self::Path { path } => path.display_lossy(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandSpecV2 {
    pub id: String,
    pub kind: String,
    pub args: Vec<CommandArgV2>,
    pub cwd: RawPath,
    pub timeout_ms: u64,
    pub owner_packages: Vec<String>,
    pub hash_paths: Vec<RawPath>,
    pub reason: String,
}

impl CommandSpecV2 {
    pub fn display_lossy(&self) -> String {
        self.args
            .iter()
            .map(CommandArgV2::display_lossy)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SkippedDecision {
    pub item: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanEnvelopeV2 {
    pub schema_version: u64,
    pub producer: String,
    pub mode: PlanMode,
    pub invocation_id: String,
    pub scope: Option<ScopeV2>,
    pub commands: Vec<CommandSpecV2>,
    pub skipped: Vec<SkippedDecision>,
    pub verdict: Option<Verdict>,
    pub enabled_expansions: Vec<String>,
    pub cache_miss_reasons: Vec<String>,
}

impl PlanEnvelopeV2 {
    pub fn new(mode: PlanMode, invocation_id: impl Into<String>) -> Self {
        Self {
            schema_version: VERIFY_LOCAL_V2_SCHEMA_VERSION,
            producer: VERIFY_LOCAL_JSON_PRODUCER.to_string(),
            mode,
            invocation_id: invocation_id.into(),
            scope: None,
            commands: Vec::new(),
            skipped: Vec::new(),
            verdict: None,
            enabled_expansions: Vec::new(),
            cache_miss_reasons: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogState {
    Complete,
    IncompleteAfterTermination,
    IoFailure,
    FramingFailure,
    IntegrityFailure,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchErrorKind {
    CommandNotFound,
    PermissionDenied,
    UnsupportedPath,
    Other,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactOutputArtifactV2 {
    pub sha256: String,
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutputOmissionV2 {
    pub bytes: u64,
    pub lines: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandResultV2 {
    pub schema_version: u64,
    pub invocation_id: String,
    pub command_id: String,
    pub command_ordinal: usize,
    pub runner_nonce: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub duration_ns: u64,
    pub timed_out: bool,
    pub cancelled: bool,
    pub runner_error: Option<String>,
    pub launch_error: Option<LaunchErrorKind>,
    pub log_state: LogState,
    pub log_path: Option<PathBuf>,
    pub diagnostic: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_output_artifact: Option<ExactOutputArtifactV2>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic_omission: Option<OutputOmissionV2>,
    pub cached: bool,
    pub flaky: bool,
    pub baseline: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FinalizedCommandResult {
    pub raw: CommandResultV2,
    pub status: Verdict,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FinalizedVerification {
    pub plan: PlanEnvelopeV2,
    pub results: Vec<FinalizedCommandResult>,
    pub verdict: Verdict,
    pub exit_code: i32,
    pub cache_eligible: bool,
    pub finalization_error: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlanRequest {
    pub mode: Option<PlanMode>,
    pub changed: Vec<RawPath>,
    pub staged: bool,
    pub all_dirty: bool,
    pub scope_current: bool,
    pub related: bool,
    pub related_tests: bool,
    pub allow_workspace: bool,
    pub isolated: bool,
    pub regen: bool,
    pub baseline: bool,
    pub no_cache: bool,
    pub cache_readonly: bool,
    pub retry_flakes: bool,
    pub scope_start: Option<String>,
    pub scope_add: Vec<RawPath>,
    pub scope_reset: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SnapshotSource {
    #[default]
    Worktree,
    ExplicitPaths,
    CommitDiff {
        base: Option<String>,
        head: Option<String>,
        merge_base: Option<String>,
        pull_request: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SnapshotRecord {
    pub status: String,
    pub path: RawPath,
    pub original_path: Option<RawPath>,
    pub staged: bool,
    pub unstaged: bool,
    pub submodule_state: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RepositorySnapshot {
    #[serde(skip)]
    pub repository_root: Option<PathBuf>,
    pub source: SnapshotSource,
    pub records: Vec<SnapshotRecord>,
    pub complete: bool,
    pub fallback_reasons: Vec<String>,
}
