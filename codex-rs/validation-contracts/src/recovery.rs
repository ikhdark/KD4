use crate::canonical::ContractError;
use crate::canonical::Sha256HexV1;
use crate::canonical::proof_hash;
use crate::canonical::validate_nonempty_nfc;
use crate::canonical::validate_sorted_unique_nfc_strings;
use crate::path::StrictRepositoryPathV1;
use crate::runner::TestRouteIdV1;
use crate::selection::ExecutableIdentityV1;
use crate::selection::TestIdV1;
use crate::selection::ValidationIdV1;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecoveryKindV1 {
    Unittest,
    Doctest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenSourceAuthorityV1 {
    pub baseline_commit: String,
    pub repository_identity_sha256: Sha256HexV1,
    pub source_tree_sha256: Sha256HexV1,
}

pub const DOCTEST_RECAPTURE_BASELINE_COMMIT: &str = "60bb133fa0a4f25e83851ab16d8c462e5f42ff95";
pub const DOCTEST_RECAPTURE_SOURCE_TREE_SHA256: &str =
    "654591dd1ddda7a77312172ec7c70e60e80990590c7c74a7c3b08a445279d90e";
pub const DOCTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256: &str =
    "f386e4786f3a61829ecdd61e764fa9d65eddbd08f2902745c6480bce448573cc";
pub const DOCTEST_RECAPTURE_TOOLCHAIN: &str = "1.95.0-x86_64-pc-windows-msvc";
pub const UNITTEST_RECAPTURE_BASELINE_COMMIT: &str = DOCTEST_RECAPTURE_BASELINE_COMMIT;
pub const UNITTEST_RECAPTURE_SOURCE_TREE_SHA256: &str = DOCTEST_RECAPTURE_SOURCE_TREE_SHA256;
pub const UNITTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256: &str =
    DOCTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256;
pub const UNITTEST_RECAPTURE_FROZEN_INVENTORY_RAW_SHA256: &str =
    "df230a7683f0f31f1aae4d3f7644af39cec67b09fadf8f3f1e6c60729d18196a";
pub const UNITTEST_RECAPTURE_PARENT_RECORDS_SHA256: &str =
    "a46a941721c872655dcb1c4ca55c070b9f48008a451d0df283f2d69957c2dd07";
pub const UNITTEST_FROZEN_LEDGER_PARENT_COUNT: usize = 909;
pub const UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT: usize = 893;
pub const UNITTEST_HIDDEN_AT_FREEZE_PARENT_COUNT: usize = 16;
// The frozen inventory recorded the dirty workspace fingerprint, but not the
// authenticated overlay bytes required to reproduce the executable 893-parent
// source tree. The other 16 frozen ledger identities were admitted later as
// hidden-at-freeze replacements and are not recapture executions or children.
// Keep this authority absent until that provenance is independently recovered;
// recapture packets must fail closed in the meantime.
pub const UNITTEST_RECAPTURE_SOURCE_SITE_MANIFEST_SHA256: Option<&str> = None;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctestToolIdentityV1 {
    pub executable_path: String,
    pub executable_sha256: Sha256HexV1,
    pub version_verbose_base64: String,
    pub version_verbose_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctestToolchainV1 {
    pub cargo: DoctestToolIdentityV1,
    pub name: String,
    pub rustc: DoctestToolIdentityV1,
    pub rustdoc: DoctestToolIdentityV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctestSourceIsolationV1 {
    pub archive_command: Vec<String>,
    pub archive_sha256: Sha256HexV1,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctestRawOccurrenceV1 {
    pub global_ordinal: u64,
    pub parent_baseline_id: String,
    pub parent_ordinal: u64,
    pub raw_listing_line: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctestPackageRunV1 {
    pub command_argv: Vec<String>,
    pub exit_code: u64,
    pub package_name: String,
    pub raw_occurrences: Vec<DoctestRawOccurrenceV1>,
    pub selected_count: u64,
    pub stderr_base64: String,
    pub stderr_sha256: Sha256HexV1,
    pub stdout_base64: String,
    pub stdout_sha256: Sha256HexV1,
    pub target_id: String,
    pub working_directory: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctestParentCountV1 {
    pub parent_baseline_id: String,
    pub raw_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctestRecapturePacketV1 {
    pub attempt_id: String,
    pub baseline_commit: String,
    pub format_id: String,
    pub parent_counts: Vec<DoctestParentCountV1>,
    pub raw_occurrence_count: u64,
    pub receipt_sha256: Sha256HexV1,
    pub repository_identity_sha256: Sha256HexV1,
    pub runs: Vec<DoctestPackageRunV1>,
    pub schema_version: u8,
    pub source_isolation: DoctestSourceIsolationV1,
    pub source_tree_sha256: Sha256HexV1,
    pub toolchain: DoctestToolchainV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestArtifactV1 {
    pub base64: String,
    pub byte_count: u64,
    pub sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestArtifactsV1 {
    pub parent_manifest: UnittestArtifactV1,
    pub report: UnittestArtifactV1,
    pub stderr: UnittestArtifactV1,
    pub stdout: UnittestArtifactV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestCloneConfigV1 {
    pub core_autocrlf: bool,
    pub git_hooks_path: String,
    pub hardlinks: bool,
    pub local: bool,
    pub no_checkout: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestSourceIsolationV1 {
    pub checkout_command: Vec<String>,
    pub clean_after: bool,
    pub clean_before: bool,
    pub clone_command: Vec<String>,
    pub clone_config: UnittestCloneConfigV1,
    pub clone_kind: String,
    pub clone_source_path: String,
    pub core_autocrlf: bool,
    pub execution_working_directory: String,
    pub git_hooks_disabled: bool,
    pub global_git_config_disabled: bool,
    pub head_commit: String,
    pub isolated_checkout_path: String,
    pub kind: String,
    pub source_repository_identity_sha256: Sha256HexV1,
    pub source_tree_sha256: Sha256HexV1,
    pub system_git_config_disabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestPythonIdentityV1 {
    pub executable_path: String,
    pub executable_sha256: Sha256HexV1,
    pub implementation: String,
    pub major: u64,
    pub micro: u64,
    pub minor: u64,
    pub soabi: String,
    pub version_base64: String,
    pub version_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestWorkerIdentityV1 {
    pub command_argv: Vec<String>,
    pub environment_sha256: Sha256HexV1,
    pub worker_id: String,
    pub worker_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestProxyEnvironmentV1 {
    #[serde(rename = "ALL_PROXY")]
    pub all_proxy_upper: Option<String>,
    #[serde(rename = "HTTPS_PROXY")]
    pub https_proxy_upper: Option<String>,
    #[serde(rename = "HTTP_PROXY")]
    pub http_proxy_upper: Option<String>,
    #[serde(rename = "NO_PROXY")]
    pub no_proxy_upper: Option<String>,
    pub all_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub http_proxy: Option<String>,
    pub no_proxy: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestNetworkIsolationV1 {
    pub codex_executable_path: String,
    pub codex_executable_sha256: Sha256HexV1,
    pub codex_network_allow_local_binding: String,
    pub command_argv: Vec<String>,
    pub fail_closed: bool,
    pub kind: String,
    pub profile: String,
    pub proxy_environment: UnittestProxyEnvironmentV1,
    pub sandbox_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestParentRecordV1 {
    pub baseline_id: String,
    pub native_id: String,
    pub predecessor_entry_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestSourceSiteV1 {
    pub column: u64,
    pub declared_site_id: String,
    pub line: u64,
    pub parent_baseline_id: String,
    pub path: StrictRepositoryPathV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestSourceAuditV1 {
    pub embedded_non_ast_marker_count: u64,
    pub executable_ast_site_count: u64,
    pub source_file_count: u64,
    pub source_site_manifest_sha256: Sha256HexV1,
    pub textual_marker_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestSubtestOccurrenceV1 {
    pub canonical_context_projection: CanonicalParameterProjectionV1,
    pub declared_site_id: String,
    pub occurrence_ordinal: u64,
    pub parent_baseline_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestParentManifestV1 {
    pub manifest_sha256: Sha256HexV1,
    pub method_body_observed: bool,
    pub parent_baseline_id: String,
    pub site_ids: Vec<String>,
    pub subtest_occurrence_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestOutputBindingV1 {
    pub parent_baseline_id: String,
    pub parent_result_sha256: Sha256HexV1,
    pub report_sha256: Sha256HexV1,
    pub stderr_sha256: Sha256HexV1,
    pub stdout_sha256: Sha256HexV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnittestTerminalResultV1 {
    Passed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestParentResultV1 {
    pub parent_baseline_id: String,
    pub selected: bool,
    pub skip_reason: Option<String>,
    pub started: bool,
    pub terminal_result: UnittestTerminalResultV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestTotalCountsV1 {
    pub method_body_child_count: u64,
    pub parent_record_count: u64,
    pub recovered_child_count: u64,
    pub selected_parent_count: u64,
    pub started_parent_count: u64,
    pub subtest_occurrence_count: u64,
    pub terminal_parent_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestOutputBindingResultV1 {
    pub parent_baseline_id: String,
    pub parent_result_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestParentManifestArtifactV1 {
    pub baseline_commit: String,
    pub format_id: String,
    pub frozen_inventory_raw_sha256: Sha256HexV1,
    pub parent_records: Vec<UnittestParentRecordV1>,
    pub schema_version: u8,
    pub source_tree_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestExecutionReportV1 {
    pub format_id: String,
    pub frozen_inventory_raw_sha256: Sha256HexV1,
    pub output_binding_results: Vec<UnittestOutputBindingResultV1>,
    pub parent_manifest_sha256: Sha256HexV1,
    pub parent_results: Vec<UnittestParentResultV1>,
    pub schema_version: u8,
    pub selection: UnittestExecutionSelectionV1,
    pub socket_policy: String,
    pub source_site_manifest: Vec<UnittestSourceSiteV1>,
    pub subtest_occurrences: Vec<UnittestSubtestOccurrenceV1>,
    pub total_counts: UnittestExecutionCountsV1,
    pub untrusted_observations: UnittestUntrustedObservationsV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestExecutionSelectionV1 {
    pub intended_count: u64,
    pub intended_native_ids: Vec<String>,
    pub selected_count: u64,
    pub selected_native_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestUntrustedCheckoutObservationV1 {
    pub clean_after: bool,
    pub clean_before: bool,
    pub execution_working_directory: String,
    pub head_commit: String,
    pub source_tree_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestExecutionCountsV1 {
    pub selected_parent_count: u64,
    pub started_parent_count: u64,
    pub subtest_occurrence_count: u64,
    pub terminal_parent_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestUntrustedEnvironmentObservationV1 {
    pub codex_network_allow_local_binding: String,
    pub proxy_environment: UnittestProxyEnvironmentV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestUntrustedProcessObservationV1 {
    pub command_argv: Vec<String>,
    pub python_executable_path: String,
    pub python_executable_sha256: Sha256HexV1,
    pub worker_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestUntrustedObservationsV1 {
    pub checkout: UnittestUntrustedCheckoutObservationV1,
    pub environment: UnittestUntrustedEnvironmentObservationV1,
    pub process: UnittestUntrustedProcessObservationV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnittestRecapturePacketV1 {
    pub artifacts: UnittestArtifactsV1,
    pub attempt_id: String,
    pub baseline_commit: String,
    pub format_id: String,
    pub frozen_inventory_raw_sha256: Sha256HexV1,
    pub network_isolation: UnittestNetworkIsolationV1,
    pub output_bindings: Vec<UnittestOutputBindingV1>,
    pub parent_manifests: Vec<UnittestParentManifestV1>,
    pub parent_records: Vec<UnittestParentRecordV1>,
    pub parent_results: Vec<UnittestParentResultV1>,
    pub python_identity: UnittestPythonIdentityV1,
    pub receipt_sha256: Sha256HexV1,
    pub repository_identity_sha256: Sha256HexV1,
    pub schema_version: u8,
    pub source_audit: UnittestSourceAuditV1,
    pub source_isolation: UnittestSourceIsolationV1,
    pub source_site_manifest: Vec<UnittestSourceSiteV1>,
    pub source_tree_sha256: Sha256HexV1,
    pub subtest_occurrences: Vec<UnittestSubtestOccurrenceV1>,
    pub total_counts: UnittestTotalCountsV1,
    pub worker_identity: UnittestWorkerIdentityV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubtestSiteObservationV1 {
    pub column: u64,
    pub line: u64,
    pub parent_id: String,
    pub path: StrictRepositoryPathV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RecoveryLegacyEvidenceV1 {
    Unittest {
        frozen_parent_count: u64,
        historical_subtest_call_count: Option<u64>,
    },
    Doctest {
        frozen_unique_count: u64,
        historical_raw_count: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RecoveryCurrentAuditV1 {
    Unittest {
        executable_ast_call_count: u64,
        excluded_embedded_fixture_count: u64,
        runner_site_observations: Vec<SubtestSiteObservationV1>,
        text_call_count: u64,
    },
    Doctest {
        declared_count: u64,
        raw_count: Option<u64>,
        unique_count: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RecoveryPendingRequirementV1 {
    Unittest {
        baseline_commit: String,
        expected_parent_output_sha256s: Vec<ParentRecaptureOutputV1>,
        reasons: Vec<String>,
        required_parent_ids: Vec<String>,
        required_parent_count: u64,
    },
    Doctest {
        baseline_commit: String,
        reasons: Vec<String>,
        required_package_targets: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentRecaptureOutputV1 {
    pub output_sha256: Sha256HexV1,
    pub parent_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveredChildKindV1 {
    UnittestMethodBody,
    UnittestSubtest,
    Doctest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CanonicalParameterProjectionV1 {
    Null,
    Boolean {
        value: bool,
    },
    Integer {
        value: i64,
    },
    String {
        value: String,
    },
    RepositoryPath {
        value: StrictRepositoryPathV1,
    },
    Bytes {
        base64url: String,
    },
    List {
        items: Vec<CanonicalParameterProjectionV1>,
    },
    Tuple {
        items: Vec<CanonicalParameterProjectionV1>,
    },
    Set {
        items: Vec<CanonicalParameterProjectionV1>,
    },
    Mapping {
        entries: Vec<CanonicalParameterMappingEntryV1>,
    },
    Enum {
        type_name: String,
        variant: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalParameterMappingEntryV1 {
    pub key: CanonicalParameterProjectionV1,
    pub value: CanonicalParameterProjectionV1,
}

impl CanonicalParameterProjectionV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Null | Self::Boolean { .. } | Self::Integer { .. } => Ok(()),
            Self::String { value } => crate::canonical::validate_nfc(value),
            Self::RepositoryPath { .. } => Ok(()),
            Self::Bytes { base64url } => {
                if base64url.contains('=')
                    || base64url
                        .bytes()
                        .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'_'))
                {
                    Err(ContractError::InvalidContract(
                        "bytes parameter must use unpadded base64url".to_owned(),
                    ))
                } else {
                    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(base64url)
                        .map_err(|_| {
                            ContractError::InvalidContract(
                                "bytes parameter must be valid unpadded base64url".to_owned(),
                            )
                        })?;
                    if base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(decoded)
                        != *base64url
                    {
                        return Err(ContractError::InvalidContract(
                            "bytes parameter must be canonical unpadded base64url".to_owned(),
                        ));
                    }
                    Ok(())
                }
            }
            Self::List { items } | Self::Tuple { items } => {
                for item in items {
                    item.validate()?;
                }
                Ok(())
            }
            Self::Set { items } => {
                for item in items {
                    item.validate()?;
                }
                ensure_sorted_unique_jcs(items, "set parameter items")
            }
            Self::Mapping { entries } => {
                for entry in entries {
                    entry.key.validate()?;
                    entry.value.validate()?;
                }
                ensure_sorted_unique_jcs(entries, "mapping parameter entries")
            }
            Self::Enum { type_name, variant } => {
                crate::canonical::validate_identifier(type_name)?;
                crate::canonical::validate_identifier(variant)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveredChildSourceV1 {
    pub canonical_parameter_projection: Option<CanonicalParameterProjectionV1>,
    pub child_kind: RecoveredChildKindV1,
    pub declared_site_id: Option<String>,
    pub executable_identity: ExecutableIdentityV1,
    pub gap_id: String,
    pub occurrence_ordinal: Option<u64>,
    pub parent_baseline_id: String,
}

impl RecoveredChildSourceV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.recovered-child-identity.v1";

    pub fn child_hash(&self) -> Result<Sha256HexV1, ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        self.executable_identity.validate()?;
        validate_nonempty_nfc(&self.gap_id, "recovered child gap ID")?;
        validate_nonempty_nfc(&self.parent_baseline_id, "recovered child parent ID")?;
        match self.child_kind {
            RecoveredChildKindV1::UnittestMethodBody
                if self.canonical_parameter_projection.is_none()
                    && self.declared_site_id.is_none()
                    && self.occurrence_ordinal.is_none() => {}
            RecoveredChildKindV1::UnittestSubtest
                if self.canonical_parameter_projection.is_some()
                    && self.declared_site_id.is_some()
                    && self.occurrence_ordinal.is_some() => {}
            RecoveredChildKindV1::Doctest
                if self.canonical_parameter_projection.is_none()
                    && self.declared_site_id.is_none()
                    && self.occurrence_ordinal.is_some() => {}
            _ => {
                return Err(ContractError::InvalidContract(
                    "recovered child kind disagrees with site/parameter/ordinal fields".to_owned(),
                ));
            }
        }
        if let Some(parameters) = &self.canonical_parameter_projection {
            parameters.validate()?;
        }
        if let Some(site) = &self.declared_site_id {
            validate_nonempty_nfc(site, "declared subtest site ID")?;
        }
        match (&self.child_kind, &self.executable_identity) {
            (
                RecoveredChildKindV1::UnittestMethodBody | RecoveredChildKindV1::UnittestSubtest,
                ExecutableIdentityV1::Test {
                    route_id: TestRouteIdV1::PythonUnittest,
                    test_id,
                    ..
                },
            ) if test_id.as_str() == self.parent_baseline_id => {}
            (
                RecoveredChildKindV1::Doctest,
                ExecutableIdentityV1::Test {
                    route_id: TestRouteIdV1::RustDoctest,
                    test_id,
                    ..
                },
            ) if test_id.as_str()
                == format!(
                    "{}::recovered-raw-occurrence:{}",
                    self.parent_baseline_id,
                    self.occurrence_ordinal.unwrap_or(u64::MAX)
                ) => {}
            _ => {
                return Err(ContractError::InvalidContract(
                    "recovered child kind/parent/ordinal disagrees with executable identity"
                        .to_owned(),
                ));
            }
        }
        proof_hash(Self::HASH_DOMAIN, self)
    }

    pub fn effective_test_id(&self) -> Result<String, ContractError> {
        Ok(format!("inventory-v2-recovered.{}", self.child_hash()?))
    }

    pub fn recovered_child_id(&self) -> Result<String, ContractError> {
        self.effective_test_id()
    }

    pub fn obligation_id(&self) -> Result<String, ContractError> {
        self.effective_test_id()
    }
}

impl UnittestRecapturePacketV1 {
    pub const FORMAT_ID: &'static str = "kd4.unittest-recapture.v1";
    pub const PARENT_RECORD_SET_HASH_DOMAIN: &'static str =
        "kd4.unittest-recapture-parent-record-set.v1";
    pub const PARENT_RESULT_HASH_DOMAIN: &'static str = "kd4.unittest-parent-result.v1";
    pub const RECEIPT_HASH_DOMAIN: &'static str = "kd4.unittest-recapture-receipt.v1";
    pub const SOURCE_SITE_MANIFEST_HASH_DOMAIN: &'static str =
        "kd4.unittest-recapture-source-site-manifest.v1";
    pub const SOURCE_SITE_HASH_DOMAIN: &'static str = "kd4.unittest-recapture-source-site.v1";
    pub const SUBTEST_MANIFEST_HASH_DOMAIN: &'static str = "kd4.unittest-subtest-manifest.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        validate_nonempty_nfc(&self.attempt_id, "attempt ID")?;
        if self.schema_version != 1
            || self.format_id != Self::FORMAT_ID
            || self.baseline_commit != UNITTEST_RECAPTURE_BASELINE_COMMIT
            || self.frozen_inventory_raw_sha256.as_str()
                != UNITTEST_RECAPTURE_FROZEN_INVENTORY_RAW_SHA256
            || self.repository_identity_sha256.as_str()
                != UNITTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256
            || self.source_tree_sha256.as_str() != UNITTEST_RECAPTURE_SOURCE_TREE_SHA256
        {
            return Err(ContractError::InvalidContract(
                "unittest recapture frozen authority mismatch".to_owned(),
            ));
        }
        let frozen_source_site_manifest_sha256 = UNITTEST_RECAPTURE_SOURCE_SITE_MANIFEST_SHA256
            .ok_or_else(|| {
                ContractError::InvalidContract(
                    "unittest recapture freeze-overlay source authority is unavailable".to_owned(),
                )
            })?;

        validate_nonempty_nfc(
            &self.source_isolation.clone_source_path,
            "clone source path",
        )?;
        validate_nonempty_nfc(
            &self.source_isolation.isolated_checkout_path,
            "isolated checkout path",
        )?;
        validate_nonempty_nfc(
            &self.source_isolation.clone_config.git_hooks_path,
            "disabled Git hooks path",
        )?;
        let expected_hooks_config = format!(
            "core.hooksPath={}",
            self.source_isolation.clone_config.git_hooks_path
        );
        if self.source_isolation.kind != "detached-checkout"
            || self.source_isolation.clone_kind != "local-no-hardlinks-no-checkout"
            || !self
                .source_isolation
                .clone_command
                .iter()
                .map(String::as_str)
                .eq([
                    "git",
                    "clone",
                    "--local",
                    "--no-hardlinks",
                    "--no-checkout",
                    "--config",
                    "core.autocrlf=false",
                    "--config",
                    expected_hooks_config.as_str(),
                    self.source_isolation.clone_source_path.as_str(),
                    self.source_isolation.isolated_checkout_path.as_str(),
                ])
            || self.source_isolation.clone_config.core_autocrlf
            || self.source_isolation.clone_config.hardlinks
            || !self.source_isolation.clone_config.local
            || !self.source_isolation.clone_config.no_checkout
            || !self
                .source_isolation
                .checkout_command
                .iter()
                .map(String::as_str)
                .eq([
                    "git",
                    "-C",
                    self.source_isolation.isolated_checkout_path.as_str(),
                    "checkout",
                    "--detach",
                    "--force",
                    UNITTEST_RECAPTURE_BASELINE_COMMIT,
                ])
            || !self.source_isolation.clean_before
            || !self.source_isolation.clean_after
            || self.source_isolation.core_autocrlf
            || !self.source_isolation.git_hooks_disabled
            || !self.source_isolation.global_git_config_disabled
            || !self.source_isolation.system_git_config_disabled
            || self.source_isolation.execution_working_directory != "."
            || self.source_isolation.head_commit != UNITTEST_RECAPTURE_BASELINE_COMMIT
            || self
                .source_isolation
                .source_repository_identity_sha256
                .as_str()
                != UNITTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256
            || self.source_isolation.source_tree_sha256.as_str()
                != UNITTEST_RECAPTURE_SOURCE_TREE_SHA256
        {
            return Err(ContractError::InvalidContract(
                "unittest recapture was not an isolated clean checkout".to_owned(),
            ));
        }
        if self.python_identity.implementation != "CPython" {
            return Err(ContractError::InvalidContract(
                "unittest recapture requires CPython".to_owned(),
            ));
        }
        validate_nonempty_nfc(
            &self.python_identity.executable_path,
            "Python executable path",
        )?;
        validate_nonempty_nfc(&self.python_identity.soabi, "Python SOABI")?;
        decode_artifact(
            &self.python_identity.version_base64,
            &self.python_identity.version_sha256,
            "Python version",
        )?;
        validate_nonempty_nfc(&self.worker_identity.worker_id, "worker ID")?;
        if self.worker_identity.command_argv.is_empty() {
            return Err(ContractError::InvalidContract(
                "unittest worker command must be nonempty".to_owned(),
            ));
        }
        for argument in &self.worker_identity.command_argv {
            validate_nonempty_nfc(argument, "worker command argument")?;
        }
        let network = &self.network_isolation;
        validate_nonempty_nfc(&network.codex_executable_path, "Codex executable path")?;
        if network.kind != "codex-windows-sandbox"
            || network.profile != ":workspace"
            || !network.sandbox_available
            || !network.fail_closed
            || network.command_argv.len() < 8
            || network.command_argv.iter().take(5).map(String::as_str).ne([
                network.codex_executable_path.as_str(),
                "sandbox",
                "-P",
                ":workspace",
                "-C",
            ])
            || network.command_argv.get(5).map(String::as_str)
                != Some(self.source_isolation.isolated_checkout_path.as_str())
            || network.command_argv.get(6).map(String::as_str) != Some("--")
            || network
                .command_argv
                .iter()
                .skip(7)
                .ne(self.worker_identity.command_argv.iter())
            || self
                .worker_identity
                .command_argv
                .first()
                .map(String::as_str)
                != Some(self.python_identity.executable_path.as_str())
        {
            return Err(ContractError::InvalidContract(
                "unittest worker did not use the exact public Codex sandbox boundary".to_owned(),
            ));
        }
        for argument in &network.command_argv {
            validate_nonempty_nfc(argument, "sandbox command argument")?;
        }
        if network.proxy_environment
            != (UnittestProxyEnvironmentV1 {
                all_proxy_upper: None,
                https_proxy_upper: None,
                http_proxy_upper: None,
                no_proxy_upper: None,
                all_proxy: None,
                https_proxy: None,
                http_proxy: None,
                no_proxy: None,
            })
            || network.codex_network_allow_local_binding != "1"
        {
            return Err(ContractError::InvalidContract(
                "unittest sandbox must clear proxies and explicitly allow loopback binding"
                    .to_owned(),
            ));
        }

        if self.parent_records.len() != UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT {
            return Err(ContractError::InvalidContract(
                "unittest recapture must bind exactly 893 executable parents".to_owned(),
            ));
        }
        let mut parent_ids = Vec::with_capacity(self.parent_records.len());
        for record in &self.parent_records {
            validate_nonempty_nfc(&record.baseline_id, "unittest baseline ID")?;
            validate_nonempty_nfc(&record.native_id, "unittest native ID")?;
            if !record
                .baseline_id
                .ends_with(&format!("python-unittest::{}", record.native_id))
            {
                return Err(ContractError::InvalidContract(
                    "unittest parent native identity mismatch".to_owned(),
                ));
            }
            parent_ids.push(record.baseline_id.clone());
        }
        ensure_sorted_unique(&parent_ids, "unittest parent records")?;
        let parent_record_set_sha256 =
            proof_hash(Self::PARENT_RECORD_SET_HASH_DOMAIN, &self.parent_records)?;
        if parent_record_set_sha256.as_str() != UNITTEST_RECAPTURE_PARENT_RECORDS_SHA256 {
            return Err(ContractError::InvalidContract(
                "unittest parent record set is not the frozen executable 893-parent set".to_owned(),
            ));
        }

        let parent_set = parent_ids
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        let mut site_by_id = std::collections::BTreeMap::<&str, &UnittestSourceSiteV1>::new();
        let mut site_ids = Vec::with_capacity(self.source_site_manifest.len());
        for site in &self.source_site_manifest {
            validate_nonempty_nfc(&site.declared_site_id, "declared unittest site ID")?;
            if site.line == 0
                || site.column == 0
                || !parent_set.contains(site.parent_baseline_id.as_str())
                || site.declared_site_id != unittest_source_site_id_v1(site)?
                || site_by_id
                    .insert(site.declared_site_id.as_str(), site)
                    .is_some()
            {
                return Err(ContractError::InvalidContract(
                    "unittest source site identity, parent, or coordinates mismatch".to_owned(),
                ));
            }
            site_ids.push(site.declared_site_id.clone());
        }
        ensure_sorted_unique(&site_ids, "unittest source-site manifest")?;
        let source_site_manifest_sha256 = proof_hash(
            Self::SOURCE_SITE_MANIFEST_HASH_DOMAIN,
            &self.source_site_manifest,
        )?;
        let distinct_source_paths = self
            .source_site_manifest
            .iter()
            .map(|site| &site.path)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        if self.source_audit.embedded_non_ast_marker_count != 1
            || self.source_audit.executable_ast_site_count != 68
            || self.source_audit.source_file_count != 22
            || self.source_audit.source_site_manifest_sha256.as_str()
                != frozen_source_site_manifest_sha256
            || self.source_audit.textual_marker_count != 69
            || source_site_manifest_sha256.as_str() != frozen_source_site_manifest_sha256
            || self.source_site_manifest.len() != 68
            || distinct_source_paths != 22
        {
            return Err(ContractError::InvalidContract(
                "unittest recapture source audit mismatch".to_owned(),
            ));
        }

        let mut occurrence_keys = Vec::with_capacity(self.subtest_occurrences.len());
        let mut occurrences_by_parent = parent_ids
            .iter()
            .map(|parent| (parent.as_str(), Vec::<&UnittestSubtestOccurrenceV1>::new()))
            .collect::<std::collections::BTreeMap<_, _>>();
        for occurrence in &self.subtest_occurrences {
            occurrence.canonical_context_projection.validate()?;
            let site = site_by_id
                .get(occurrence.declared_site_id.as_str())
                .ok_or_else(|| {
                    ContractError::InvalidContract(
                        "unittest subtest occurrence has an unknown parent/site".to_owned(),
                    )
                })?;
            let parent_occurrences = occurrences_by_parent
                .get_mut(occurrence.parent_baseline_id.as_str())
                .ok_or_else(|| {
                    ContractError::InvalidContract(
                        "unittest subtest occurrence has an unknown parent/site".to_owned(),
                    )
                })?;
            if site.parent_baseline_id != occurrence.parent_baseline_id {
                return Err(ContractError::InvalidContract(
                    "unittest subtest occurrence has an unknown parent/site".to_owned(),
                ));
            }
            occurrence_keys.push((
                occurrence.parent_baseline_id.as_str(),
                occurrence.declared_site_id.as_str(),
                occurrence.occurrence_ordinal,
            ));
            parent_occurrences.push(occurrence);
        }
        if occurrence_keys.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ContractError::InvalidContract(
                "unittest subtest occurrences must be sorted and unique".to_owned(),
            ));
        }
        let mut ordinal_groups = std::collections::BTreeMap::<(&str, &str), Vec<u64>>::new();
        for (parent, site, ordinal) in occurrence_keys {
            ordinal_groups
                .entry((parent, site))
                .or_default()
                .push(ordinal);
        }
        if ordinal_groups
            .values()
            .any(|ordinals| ordinals.iter().copied().ne(0..ordinals.len() as u64))
        {
            return Err(ContractError::InvalidContract(
                "unittest subtest ordinals must be contiguous per parent/site".to_owned(),
            ));
        }

        let expected_manifests = derive_unittest_manifests_v1(&parent_ids, &occurrences_by_parent)?;
        if self.parent_manifests != expected_manifests {
            return Err(ContractError::InvalidContract(
                "unittest parent manifests do not match occurrences".to_owned(),
            ));
        }

        let parent_manifest_raw = self
            .artifacts
            .parent_manifest
            .validate("unittest parent_manifest")?;
        let report_raw = self.artifacts.report.validate("unittest report")?;
        self.artifacts.stderr.validate("unittest stderr")?;
        self.artifacts.stdout.validate("unittest stdout")?;
        let parent_manifest_value = crate::canonical::parse_canonical_jcs(&parent_manifest_raw)
            .map_err(|_| {
                ContractError::InvalidContract(
                    "unittest parent manifest artifact must be canonical JSON".to_owned(),
                )
            })?;
        let parent_manifest: UnittestParentManifestArtifactV1 =
            serde_json::from_value(parent_manifest_value).map_err(|_| {
                ContractError::InvalidContract(
                    "unittest parent manifest does not bind the frozen parent records".to_owned(),
                )
            })?;
        if parent_manifest
            != (UnittestParentManifestArtifactV1 {
                baseline_commit: UNITTEST_RECAPTURE_BASELINE_COMMIT.to_owned(),
                format_id: "kd4.unittest-parent-manifest.v1".to_owned(),
                frozen_inventory_raw_sha256: self.frozen_inventory_raw_sha256.clone(),
                parent_records: self.parent_records.clone(),
                schema_version: 1,
                source_tree_sha256: self.source_tree_sha256.clone(),
            })
        {
            return Err(ContractError::InvalidContract(
                "unittest parent manifest does not bind the frozen parent records".to_owned(),
            ));
        }
        if self.parent_results.len() != UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT
            || self.output_bindings.len() != UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT
        {
            return Err(ContractError::InvalidContract(
                "unittest results/output bindings must cover 893 executable parents".to_owned(),
            ));
        }
        let mut binding_ids = Vec::with_capacity(self.output_bindings.len());
        let mut binding_by_parent =
            std::collections::BTreeMap::<&str, &UnittestOutputBindingV1>::new();
        for binding in &self.output_bindings {
            if !parent_set.contains(binding.parent_baseline_id.as_str())
                || binding.report_sha256 != self.artifacts.report.sha256
                || binding.stderr_sha256 != self.artifacts.stderr.sha256
                || binding.stdout_sha256 != self.artifacts.stdout.sha256
                || binding_by_parent
                    .insert(binding.parent_baseline_id.as_str(), binding)
                    .is_some()
            {
                return Err(ContractError::InvalidContract(
                    "unittest output binding parent or artifact mismatch".to_owned(),
                ));
            }
            binding_ids.push(binding.parent_baseline_id.clone());
        }
        if binding_ids != parent_ids {
            return Err(ContractError::InvalidContract(
                "unittest output bindings must be parent sorted".to_owned(),
            ));
        }

        let mut terminal_count = 0_u64;
        for (result, parent) in self.parent_results.iter().zip(&parent_ids) {
            if result.parent_baseline_id != *parent || !result.selected || !result.started {
                return Err(ContractError::InvalidContract(
                    "unittest parent was not exactly selected and started".to_owned(),
                ));
            }
            if result.terminal_result != UnittestTerminalResultV1::Passed
                || result.skip_reason.is_some()
            {
                return Err(ContractError::InvalidContract(
                    "every unittest parent must pass without a skip reason".to_owned(),
                ));
            }
            let result_sha256 = proof_hash(Self::PARENT_RESULT_HASH_DOMAIN, result)?;
            if binding_by_parent[parent.as_str()].parent_result_sha256 != result_sha256 {
                return Err(ContractError::InvalidContract(
                    "unittest output binding parent result mismatch".to_owned(),
                ));
            }
            terminal_count += 1;
        }

        let report_value = crate::canonical::parse_canonical_jcs(&report_raw).map_err(|_| {
            ContractError::InvalidContract(
                "unittest report artifact must be canonical JSON".to_owned(),
            )
        })?;
        let report: UnittestExecutionReportV1 =
            serde_json::from_value(report_value).map_err(|_| {
                ContractError::InvalidContract(
                    "unittest report semantic execution content mismatch".to_owned(),
                )
            })?;
        let expected_report = UnittestExecutionReportV1 {
            format_id: "kd4.unittest-execution-report.v1".to_owned(),
            frozen_inventory_raw_sha256: self.frozen_inventory_raw_sha256.clone(),
            output_binding_results: self
                .output_bindings
                .iter()
                .map(|binding| UnittestOutputBindingResultV1 {
                    parent_baseline_id: binding.parent_baseline_id.clone(),
                    parent_result_sha256: binding.parent_result_sha256.clone(),
                })
                .collect(),
            parent_manifest_sha256: self.artifacts.parent_manifest.sha256.clone(),
            parent_results: self.parent_results.clone(),
            schema_version: 1,
            selection: UnittestExecutionSelectionV1 {
                intended_count: UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT as u64,
                intended_native_ids: self
                    .parent_records
                    .iter()
                    .map(|record| record.native_id.clone())
                    .collect(),
                selected_count: UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT as u64,
                selected_native_ids: self
                    .parent_records
                    .iter()
                    .map(|record| record.native_id.clone())
                    .collect(),
            },
            socket_policy: "loopback-only".to_owned(),
            source_site_manifest: self.source_site_manifest.clone(),
            subtest_occurrences: self.subtest_occurrences.clone(),
            total_counts: UnittestExecutionCountsV1 {
                selected_parent_count: UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT as u64,
                started_parent_count: UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT as u64,
                subtest_occurrence_count: self.subtest_occurrences.len() as u64,
                terminal_parent_count: terminal_count,
            },
            untrusted_observations: UnittestUntrustedObservationsV1 {
                checkout: UnittestUntrustedCheckoutObservationV1 {
                    clean_after: self.source_isolation.clean_after,
                    clean_before: self.source_isolation.clean_before,
                    execution_working_directory: self
                        .source_isolation
                        .execution_working_directory
                        .clone(),
                    head_commit: self.source_isolation.head_commit.clone(),
                    source_tree_sha256: self.source_isolation.source_tree_sha256.clone(),
                },
                environment: UnittestUntrustedEnvironmentObservationV1 {
                    codex_network_allow_local_binding: self
                        .network_isolation
                        .codex_network_allow_local_binding
                        .clone(),
                    proxy_environment: self.network_isolation.proxy_environment.clone(),
                },
                process: UnittestUntrustedProcessObservationV1 {
                    command_argv: self.worker_identity.command_argv.clone(),
                    python_executable_path: self.python_identity.executable_path.clone(),
                    python_executable_sha256: self.python_identity.executable_sha256.clone(),
                    worker_sha256: self.worker_identity.worker_sha256.clone(),
                },
            },
        };
        if report != expected_report {
            return Err(ContractError::InvalidContract(
                "unittest report semantic execution content mismatch".to_owned(),
            ));
        }

        let subtest_occurrence_count = self.subtest_occurrences.len() as u64;
        let expected_counts = UnittestTotalCountsV1 {
            method_body_child_count: UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT as u64,
            parent_record_count: UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT as u64,
            recovered_child_count: UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT as u64
                + subtest_occurrence_count,
            selected_parent_count: UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT as u64,
            started_parent_count: UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT as u64,
            subtest_occurrence_count,
            terminal_parent_count: terminal_count,
        };
        if self.total_counts != expected_counts {
            return Err(ContractError::InvalidContract(
                "unittest recapture total counts mismatch".to_owned(),
            ));
        }
        let expected_receipt = proof_hash(
            Self::RECEIPT_HASH_DOMAIN,
            &UnittestRecaptureReceiptProjectionV1 {
                artifacts: &self.artifacts,
                attempt_id: &self.attempt_id,
                baseline_commit: &self.baseline_commit,
                format_id: &self.format_id,
                frozen_inventory_raw_sha256: &self.frozen_inventory_raw_sha256,
                network_isolation: &self.network_isolation,
                output_bindings: &self.output_bindings,
                parent_manifests: &self.parent_manifests,
                parent_records: &self.parent_records,
                parent_results: &self.parent_results,
                python_identity: &self.python_identity,
                repository_identity_sha256: &self.repository_identity_sha256,
                schema_version: self.schema_version,
                source_audit: &self.source_audit,
                source_isolation: &self.source_isolation,
                source_site_manifest: &self.source_site_manifest,
                source_tree_sha256: &self.source_tree_sha256,
                subtest_occurrences: &self.subtest_occurrences,
                total_counts: &self.total_counts,
                worker_identity: &self.worker_identity,
            },
        )?;
        if self.receipt_sha256 != expected_receipt {
            return Err(ContractError::InvalidContract(
                "unittest recapture receipt hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn subtest_manifests(&self) -> Result<Vec<UnittestParentManifestV1>, ContractError> {
        self.validate()?;
        let parent_ids = self
            .parent_records
            .iter()
            .map(|record| record.baseline_id.clone())
            .collect::<Vec<_>>();
        let mut occurrences_by_parent = parent_ids
            .iter()
            .map(|parent| (parent.as_str(), Vec::<&UnittestSubtestOccurrenceV1>::new()))
            .collect::<std::collections::BTreeMap<_, _>>();
        for occurrence in &self.subtest_occurrences {
            occurrences_by_parent
                .get_mut(occurrence.parent_baseline_id.as_str())
                .expect("validated unittest occurrence has a known parent")
                .push(occurrence);
        }
        derive_unittest_manifests_v1(&parent_ids, &occurrences_by_parent)
    }

    pub fn recovered_child_sources(&self) -> Result<Vec<RecoveredChildSourceV1>, ContractError> {
        self.validate()?;
        let validation_id = ValidationIdV1::parse("python.unittest.recapture")?;
        let mut children = Vec::with_capacity(
            self.parent_records
                .len()
                .saturating_add(self.subtest_occurrences.len()),
        );
        for record in &self.parent_records {
            children.push(RecoveredChildSourceV1 {
                canonical_parameter_projection: None,
                child_kind: RecoveredChildKindV1::UnittestMethodBody,
                declared_site_id: None,
                executable_identity: ExecutableIdentityV1::Test {
                    route_id: TestRouteIdV1::PythonUnittest,
                    test_id: TestIdV1::parse(record.baseline_id.clone())?,
                    validation_id: validation_id.clone(),
                },
                gap_id: "gap.unittest-subtest-expansion".to_owned(),
                occurrence_ordinal: None,
                parent_baseline_id: record.baseline_id.clone(),
            });
        }
        for occurrence in &self.subtest_occurrences {
            children.push(RecoveredChildSourceV1 {
                canonical_parameter_projection: Some(
                    occurrence.canonical_context_projection.clone(),
                ),
                child_kind: RecoveredChildKindV1::UnittestSubtest,
                declared_site_id: Some(occurrence.declared_site_id.clone()),
                executable_identity: ExecutableIdentityV1::Test {
                    route_id: TestRouteIdV1::PythonUnittest,
                    test_id: TestIdV1::parse(occurrence.parent_baseline_id.clone())?,
                    validation_id: validation_id.clone(),
                },
                gap_id: "gap.unittest-subtest-expansion".to_owned(),
                occurrence_ordinal: Some(occurrence.occurrence_ordinal),
                parent_baseline_id: occurrence.parent_baseline_id.clone(),
            });
        }
        let mut keyed_children = children
            .into_iter()
            .map(|child| Ok((child.obligation_id()?, child)))
            .collect::<Result<Vec<_>, ContractError>>()?;
        keyed_children.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(keyed_children.into_iter().map(|(_, child)| child).collect())
    }

    pub fn parent_recapture_outputs(&self) -> Result<Vec<ParentRecaptureOutputV1>, ContractError> {
        self.validate()?;
        Ok(self
            .parent_records
            .iter()
            .map(|record| ParentRecaptureOutputV1 {
                output_sha256: record.predecessor_entry_sha256.clone(),
                parent_id: record.baseline_id.clone(),
            })
            .collect())
    }
}

impl UnittestArtifactV1 {
    fn validate(&self, label: &str) -> Result<Vec<u8>, ContractError> {
        let raw = decode_artifact(&self.base64, &self.sha256, label)?;
        if self.byte_count != raw.len() as u64 {
            return Err(ContractError::InvalidContract(format!(
                "{label} artifact byte count mismatch"
            )));
        }
        Ok(raw)
    }
}

#[derive(Serialize)]
struct UnittestSourceSiteHashProjectionV1<'a> {
    column: u64,
    line: u64,
    parent_baseline_id: &'a str,
    path: &'a StrictRepositoryPathV1,
}

fn unittest_source_site_id_v1(site: &UnittestSourceSiteV1) -> Result<String, ContractError> {
    Ok(format!(
        "unittest-site.{}",
        proof_hash(
            UnittestRecapturePacketV1::SOURCE_SITE_HASH_DOMAIN,
            &UnittestSourceSiteHashProjectionV1 {
                column: site.column,
                line: site.line,
                parent_baseline_id: &site.parent_baseline_id,
                path: &site.path,
            },
        )?
    ))
}

#[derive(Serialize)]
struct UnittestManifestHashProjectionV1<'a, 'b> {
    method_body_observed: bool,
    occurrences: &'a [&'b UnittestSubtestOccurrenceV1],
    parent_baseline_id: &'a str,
    site_ids: &'a [String],
    subtest_occurrence_count: u64,
}

fn derive_unittest_manifests_v1(
    parent_ids: &[String],
    occurrences_by_parent: &std::collections::BTreeMap<&str, Vec<&UnittestSubtestOccurrenceV1>>,
) -> Result<Vec<UnittestParentManifestV1>, ContractError> {
    parent_ids
        .iter()
        .map(|parent| {
            let occurrences = &occurrences_by_parent[parent.as_str()];
            let site_ids = occurrences
                .iter()
                .map(|occurrence| occurrence.declared_site_id.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let subtest_occurrence_count = occurrences.len() as u64;
            let manifest_sha256 = proof_hash(
                UnittestRecapturePacketV1::SUBTEST_MANIFEST_HASH_DOMAIN,
                &UnittestManifestHashProjectionV1 {
                    method_body_observed: true,
                    occurrences,
                    parent_baseline_id: parent,
                    site_ids: &site_ids,
                    subtest_occurrence_count,
                },
            )?;
            Ok(UnittestParentManifestV1 {
                manifest_sha256,
                method_body_observed: true,
                parent_baseline_id: parent.clone(),
                site_ids,
                subtest_occurrence_count,
            })
        })
        .collect()
}

#[derive(Serialize)]
struct UnittestRecaptureReceiptProjectionV1<'a> {
    artifacts: &'a UnittestArtifactsV1,
    attempt_id: &'a str,
    baseline_commit: &'a str,
    format_id: &'a str,
    frozen_inventory_raw_sha256: &'a Sha256HexV1,
    network_isolation: &'a UnittestNetworkIsolationV1,
    output_bindings: &'a [UnittestOutputBindingV1],
    parent_manifests: &'a [UnittestParentManifestV1],
    parent_records: &'a [UnittestParentRecordV1],
    parent_results: &'a [UnittestParentResultV1],
    python_identity: &'a UnittestPythonIdentityV1,
    repository_identity_sha256: &'a Sha256HexV1,
    schema_version: u8,
    source_audit: &'a UnittestSourceAuditV1,
    source_isolation: &'a UnittestSourceIsolationV1,
    source_site_manifest: &'a [UnittestSourceSiteV1],
    source_tree_sha256: &'a Sha256HexV1,
    subtest_occurrences: &'a [UnittestSubtestOccurrenceV1],
    total_counts: &'a UnittestTotalCountsV1,
    worker_identity: &'a UnittestWorkerIdentityV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryResolutionV1 {
    pub child_sources: Vec<RecoveredChildSourceV1>,
    pub parent_recapture_outputs: Vec<ParentRecaptureOutputV1>,
    pub parent_container_ids: Vec<String>,
    pub recapture_receipt_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryTransitionReceiptV1 {
    pub authority_before_semantic_sha256: Sha256HexV1,
    pub child_obligation_ids: Vec<String>,
    pub frozen_source_authority_sha256: Sha256HexV1,
    pub parent_container_ids: Vec<String>,
    pub recapture_receipt_sha256: Sha256HexV1,
    pub receipt_sha256: Sha256HexV1,
    pub schema_version: u8,
}

impl FrozenSourceAuthorityV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.frozen-source-authority.v1";

    pub fn semantic_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        crate::canonical::validate_nfc(&self.baseline_commit)?;
        if self.baseline_commit.is_empty() {
            return Err(ContractError::InvalidContract(
                "frozen source authority requires a baseline commit".to_owned(),
            ));
        }
        proof_hash(Self::HASH_DOMAIN, self)
    }
}

impl DoctestToolIdentityV1 {
    fn validate(&self, tool_name: &str) -> Result<(), ContractError> {
        validate_nonempty_nfc(&self.executable_path, "tool executable path")?;
        let version = STANDARD.decode(&self.version_verbose_base64).map_err(|_| {
            ContractError::InvalidContract("tool version output must be standard base64".to_owned())
        })?;
        if STANDARD.encode(&version) != self.version_verbose_base64
            || format!("{:x}", Sha256::digest(&version)) != self.version_verbose_sha256.as_str()
        {
            return Err(ContractError::InvalidContract(
                "tool version output hash/base64 mismatch".to_owned(),
            ));
        }
        let version = std::str::from_utf8(&version).map_err(|_| {
            ContractError::InvalidContract("tool version output must be UTF-8".to_owned())
        })?;
        if !version.starts_with(&format!("{tool_name} 1.95.0 "))
            || !version.contains("host: x86_64-pc-windows-msvc")
        {
            return Err(ContractError::InvalidContract(format!(
                "{tool_name} is not the exact Rust 1.95.0 MSVC tool"
            )));
        }
        Ok(())
    }
}

impl DoctestRecapturePacketV1 {
    pub const FORMAT_ID: &'static str = "kd4.doctest-recapture.v1";
    pub const RECEIPT_HASH_DOMAIN: &'static str = "kd4.doctest-recapture-receipt.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        validate_nonempty_nfc(&self.attempt_id, "attempt ID")?;
        if self.schema_version != 1
            || self.format_id != Self::FORMAT_ID
            || self.baseline_commit != DOCTEST_RECAPTURE_BASELINE_COMMIT
            || self.source_tree_sha256.as_str() != DOCTEST_RECAPTURE_SOURCE_TREE_SHA256
            || self.repository_identity_sha256.as_str()
                != DOCTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256
        {
            return Err(ContractError::InvalidContract(
                "doctest recapture frozen identity mismatch".to_owned(),
            ));
        }
        let expected_archive = [
            "git",
            "archive",
            "--format=tar",
            DOCTEST_RECAPTURE_BASELINE_COMMIT,
        ];
        if self.source_isolation.kind != "git-archive"
            || self.source_isolation.archive_command != expected_archive.map(str::to_owned).to_vec()
        {
            return Err(ContractError::InvalidContract(
                "doctest recapture did not use the exact git archive".to_owned(),
            ));
        }
        if self.toolchain.name != DOCTEST_RECAPTURE_TOOLCHAIN {
            return Err(ContractError::InvalidContract(
                "doctest recapture toolchain mismatch".to_owned(),
            ));
        }
        self.toolchain.cargo.validate("cargo")?;
        self.toolchain.rustc.validate("rustc")?;
        self.toolchain.rustdoc.validate("rustdoc")?;

        let package_specs = [
            ("codex-core", "codex-core::lib::codex_core"),
            ("codex-rollout", "codex-rollout::lib::codex_rollout"),
            ("codex-state", "codex-state::lib::codex_state"),
            ("codex-tui", "codex-tui::lib::codex_tui"),
        ];
        if self.runs.len() != package_specs.len() {
            return Err(ContractError::InvalidContract(
                "doctest recapture must contain exactly four package runs".to_owned(),
            ));
        }
        let mut parent_ordinals = std::collections::BTreeMap::<String, u64>::from([
            (
                "rust-doctest::core\\src\\client.rs - client::ModelClient (line 1696)"
                    .to_owned(),
                0,
            ),
            (
                "rust-doctest::rollout\\src\\recorder.rs - recorder::RolloutRecorder (line 84)"
                    .to_owned(),
                0,
            ),
            (
                "rust-doctest::state\\src\\log_db.rs - log_db (line 10)".to_owned(),
                0,
            ),
            (
                "rust-doctest::tui\\src\\bottom_pane\\multi_select_picker.rs - bottom_pane::multi_select_picker (line 14)"
                    .to_owned(),
                0,
            ),
            (
                "rust-doctest::tui\\src\\bottom_pane\\multi_select_picker.rs - bottom_pane::multi_select_picker::MultiSelectPickerBuilder (line 706)"
                    .to_owned(),
                0,
            ),
        ]);
        let mut global_ordinal = 0_u64;
        for (run, (package_name, target_id)) in self.runs.iter().zip(package_specs) {
            let expected_command = [
                "cargo",
                "test",
                "--locked",
                "--offline",
                "-p",
                package_name,
                "--doc",
                "--",
                "--list",
                "--format",
                "terse",
            ]
            .map(str::to_owned)
            .to_vec();
            if run.package_name != package_name
                || run.target_id != target_id
                || run.command_argv != expected_command
                || run.working_directory != "codex-rs"
                || run.exit_code != 0
            {
                return Err(ContractError::InvalidContract(
                    "doctest package target, command, or exit outcome mismatch".to_owned(),
                ));
            }
            let stdout = decode_artifact(&run.stdout_base64, &run.stdout_sha256, "doctest stdout")?;
            decode_artifact(&run.stderr_base64, &run.stderr_sha256, "doctest stderr")?;
            let stdout = std::str::from_utf8(&stdout).map_err(|_| {
                ContractError::InvalidContract("doctest stdout must be UTF-8".to_owned())
            })?;
            let listing_lines = stdout
                .lines()
                .filter(|line| line.ends_with(": test"))
                .collect::<Vec<_>>();
            if run.raw_occurrences.is_empty()
                || run.selected_count != run.raw_occurrences.len() as u64
                || listing_lines.len() != run.raw_occurrences.len()
            {
                return Err(ContractError::InvalidContract(
                    "doctest package selection is zero, partial, or count-mismatched".to_owned(),
                ));
            }
            for (occurrence, listing_line) in run.raw_occurrences.iter().zip(listing_lines) {
                let normalized = listing_line
                    .strip_suffix(": test")
                    .expect("listing lines were filtered")
                    .replace('/', "\\");
                let parent_id = format!("rust-doctest::{normalized}");
                let expected_target = doctest_parent_target(&parent_id).ok_or_else(|| {
                    ContractError::InvalidContract(
                        "doctest occurrence has an unknown frozen parent".to_owned(),
                    )
                })?;
                let next_parent_ordinal = parent_ordinals.get_mut(&parent_id).ok_or_else(|| {
                    ContractError::InvalidContract(
                        "doctest occurrence has an unknown frozen parent".to_owned(),
                    )
                })?;
                if occurrence.raw_listing_line != listing_line
                    || occurrence.global_ordinal != global_ordinal
                    || occurrence.parent_baseline_id != parent_id
                    || occurrence.parent_ordinal != *next_parent_ordinal
                    || expected_target != target_id
                {
                    return Err(ContractError::InvalidContract(
                        "doctest raw occurrence order, multiplicity, or target mismatch".to_owned(),
                    ));
                }
                *next_parent_ordinal += 1;
                global_ordinal += 1;
            }
        }
        if parent_ordinals.values().any(|count| *count == 0) {
            return Err(ContractError::InvalidContract(
                "doctest recapture does not cover all five frozen parents".to_owned(),
            ));
        }
        let expected_parent_counts = parent_ordinals
            .into_iter()
            .map(|(parent_baseline_id, raw_count)| DoctestParentCountV1 {
                parent_baseline_id,
                raw_count,
            })
            .collect::<Vec<_>>();
        if self.parent_counts != expected_parent_counts
            || self.raw_occurrence_count != global_ordinal
        {
            return Err(ContractError::InvalidContract(
                "doctest recapture aggregate counts mismatch".to_owned(),
            ));
        }
        let expected_receipt = proof_hash(
            Self::RECEIPT_HASH_DOMAIN,
            &DoctestRecaptureReceiptProjectionV1 {
                attempt_id: &self.attempt_id,
                baseline_commit: &self.baseline_commit,
                format_id: &self.format_id,
                parent_counts: &self.parent_counts,
                raw_occurrence_count: self.raw_occurrence_count,
                repository_identity_sha256: &self.repository_identity_sha256,
                runs: &self.runs,
                schema_version: self.schema_version,
                source_isolation: &self.source_isolation,
                source_tree_sha256: &self.source_tree_sha256,
                toolchain: &self.toolchain,
            },
        )?;
        if self.receipt_sha256 != expected_receipt {
            return Err(ContractError::InvalidContract(
                "doctest recapture receipt hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct DoctestRecaptureReceiptProjectionV1<'a> {
    attempt_id: &'a str,
    baseline_commit: &'a str,
    format_id: &'a str,
    parent_counts: &'a [DoctestParentCountV1],
    raw_occurrence_count: u64,
    repository_identity_sha256: &'a Sha256HexV1,
    runs: &'a [DoctestPackageRunV1],
    schema_version: u8,
    source_isolation: &'a DoctestSourceIsolationV1,
    source_tree_sha256: &'a Sha256HexV1,
    toolchain: &'a DoctestToolchainV1,
}

fn decode_artifact(
    encoded: &str,
    expected_sha256: &Sha256HexV1,
    label: &str,
) -> Result<Vec<u8>, ContractError> {
    let raw = STANDARD
        .decode(encoded)
        .map_err(|_| ContractError::InvalidContract(format!("{label} must be standard base64")))?;
    if STANDARD.encode(&raw) != encoded
        || format!("{:x}", Sha256::digest(&raw)) != expected_sha256.as_str()
    {
        return Err(ContractError::InvalidContract(format!(
            "{label} artifact hash/base64 mismatch"
        )));
    }
    Ok(raw)
}

fn doctest_parent_target(parent_id: &str) -> Option<&'static str> {
    match parent_id {
        "rust-doctest::core\\src\\client.rs - client::ModelClient (line 1696)" => {
            Some("codex-core::lib::codex_core")
        }
        "rust-doctest::rollout\\src\\recorder.rs - recorder::RolloutRecorder (line 84)" => {
            Some("codex-rollout::lib::codex_rollout")
        }
        "rust-doctest::state\\src\\log_db.rs - log_db (line 10)" => {
            Some("codex-state::lib::codex_state")
        }
        "rust-doctest::tui\\src\\bottom_pane\\multi_select_picker.rs - bottom_pane::multi_select_picker (line 14)"
        | "rust-doctest::tui\\src\\bottom_pane\\multi_select_picker.rs - bottom_pane::multi_select_picker::MultiSelectPickerBuilder (line 706)" => {
            Some("codex-tui::lib::codex_tui")
        }
        _ => None,
    }
}

impl RecoveryTransitionReceiptV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.recovery-transition-receipt.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        if self.schema_version != 1 {
            return Err(ContractError::InvalidContract(
                "recovery transition receipt must be version 1".to_owned(),
            ));
        }
        ensure_sorted_unique(&self.child_obligation_ids, "transition child obligations")?;
        ensure_sorted_unique(&self.parent_container_ids, "transition parent containers")?;
        let expected = proof_hash(
            Self::HASH_DOMAIN,
            &RecoveryTransitionReceiptHashProjectionV1 {
                authority_before_semantic_sha256: &self.authority_before_semantic_sha256,
                child_obligation_ids: &self.child_obligation_ids,
                frozen_source_authority_sha256: &self.frozen_source_authority_sha256,
                parent_container_ids: &self.parent_container_ids,
                recapture_receipt_sha256: &self.recapture_receipt_sha256,
                schema_version: self.schema_version,
            },
        )?;
        if self.receipt_sha256 != expected {
            return Err(ContractError::InvalidContract(
                "recovery transition receipt hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct RecoveryTransitionReceiptHashProjectionV1<'a> {
    authority_before_semantic_sha256: &'a Sha256HexV1,
    child_obligation_ids: &'a [String],
    frozen_source_authority_sha256: &'a Sha256HexV1,
    parent_container_ids: &'a [String],
    recapture_receipt_sha256: &'a Sha256HexV1,
    schema_version: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryRecoveryRecordV1 {
    pub current_audit: RecoveryCurrentAuditV1,
    pub gap_id: String,
    pub kind: RecoveryKindV1,
    pub legacy_evidence: RecoveryLegacyEvidenceV1,
    pub pending_requirement: Option<RecoveryPendingRequirementV1>,
    pub recovery_id: String,
    pub resolution: Option<RecoveryResolutionV1>,
    pub state: RecoveryStateV1,
    pub transition_receipt_sha256: Option<Sha256HexV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryRecoveryAuthorityV1 {
    pub format_id: String,
    pub frozen_source_authority: FrozenSourceAuthorityV1,
    pub records: Vec<InventoryRecoveryRecordV1>,
    pub schema_version: u8,
    pub semantic_sha256: Sha256HexV1,
    pub self_hash: Sha256HexV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecoveryStateV1 {
    Pending,
    Resolved,
}

impl InventoryRecoveryRecordV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        validate_nonempty_nfc(&self.gap_id, "recovery gap ID")?;
        validate_nonempty_nfc(&self.recovery_id, "recovery ID")?;
        self.validate_kind_alignment()?;
        match self.state {
            RecoveryStateV1::Pending
                if self.pending_requirement.is_some()
                    && self.resolution.is_none()
                    && self.transition_receipt_sha256.is_none() =>
            {
                self.validate_pending()
            }
            RecoveryStateV1::Resolved
                if self.pending_requirement.is_none()
                    && self.resolution.is_some()
                    && self.transition_receipt_sha256.is_some() =>
            {
                self.validate_resolved()
            }
            _ => Err(ContractError::InvalidContract(
                "recovery state and nullable fields disagree".to_owned(),
            )),
        }
    }

    fn validate_kind_alignment(&self) -> Result<(), ContractError> {
        let aligned = matches!(
            (self.kind, &self.legacy_evidence, &self.current_audit),
            (
                RecoveryKindV1::Unittest,
                RecoveryLegacyEvidenceV1::Unittest { .. },
                RecoveryCurrentAuditV1::Unittest { .. }
            ) | (
                RecoveryKindV1::Doctest,
                RecoveryLegacyEvidenceV1::Doctest { .. },
                RecoveryCurrentAuditV1::Doctest { .. }
            )
        );
        if !aligned {
            return Err(ContractError::InvalidContract(
                "recovery kind, legacy evidence, and current audit disagree".to_owned(),
            ));
        }
        match (&self.legacy_evidence, &self.current_audit) {
            (
                RecoveryLegacyEvidenceV1::Unittest {
                    frozen_parent_count,
                    historical_subtest_call_count,
                },
                RecoveryCurrentAuditV1::Unittest { .. },
            ) if *frozen_parent_count == UNITTEST_FROZEN_LEDGER_PARENT_COUNT as u64
                && matches!(
                    (self.state, historical_subtest_call_count),
                    (RecoveryStateV1::Pending, None) | (RecoveryStateV1::Resolved, Some(_))
                ) => {}
            (
                RecoveryLegacyEvidenceV1::Doctest {
                    frozen_unique_count,
                    historical_raw_count,
                },
                RecoveryCurrentAuditV1::Doctest {
                    declared_count,
                    raw_count,
                    unique_count,
                },
            ) if *frozen_unique_count == 5 && *declared_count == 5 && *unique_count == 5 => {}
            _ => {
                return Err(ContractError::InvalidContract(
                    "legacy/current recovery evidence must preserve the exact pending gaps"
                        .to_owned(),
                ));
            }
        }
        if let RecoveryCurrentAuditV1::Unittest {
            executable_ast_call_count,
            excluded_embedded_fixture_count,
            runner_site_observations,
            text_call_count,
        } = &self.current_audit
        {
            for site in runner_site_observations {
                validate_nonempty_nfc(&site.parent_id, "subtest site parent ID")?;
            }
            let source_anchors = runner_site_observations
                .iter()
                .map(|site| {
                    (
                        site.line,
                        site.column,
                        site.parent_id.as_str(),
                        site.path.as_str(),
                    )
                })
                .collect::<Vec<_>>();
            if (
                *text_call_count,
                *executable_ast_call_count,
                *excluded_embedded_fixture_count,
            ) != (63, 62, 1)
                || runner_site_observations.len() != 5
                || source_anchors.iter().any(|(line, column, _, path)| {
                    *line == 0 || *column == 0 || *path != "scripts/test_rust_test_runner.py"
                })
                || source_anchors.windows(2).any(|pair| pair[0] >= pair[1])
            {
                return Err(ContractError::InvalidContract(
                    "unittest current audit must bind the exact 63/62/1 observation and five deterministic unique source anchors"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn validate_pending(&self) -> Result<(), ContractError> {
        match (self.kind, self.pending_requirement.as_ref()) {
            (
                RecoveryKindV1::Unittest,
                Some(RecoveryPendingRequirementV1::Unittest {
                    baseline_commit,
                    expected_parent_output_sha256s,
                    reasons,
                    required_parent_ids,
                    required_parent_count,
                }),
            ) => {
                if baseline_commit.is_empty()
                    || reasons.is_empty()
                    || *required_parent_count != UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT as u64
                    || required_parent_ids.len() != UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT
                    || expected_parent_output_sha256s.len()
                        != UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT
                {
                    return Err(ContractError::InvalidContract(
                        "pending unittest recovery must cover all 893 executable parents"
                            .to_owned(),
                    ));
                }
                crate::canonical::validate_nfc(baseline_commit)?;
                ensure_sorted_unique(reasons, "unittest recovery reasons")?;
                ensure_sorted_unique(required_parent_ids, "required unittest parents")?;
                let output_parents = expected_parent_output_sha256s
                    .iter()
                    .map(|entry| entry.parent_id.clone())
                    .collect::<Vec<_>>();
                ensure_sorted_unique(&output_parents, "parent output tuples")?;
                if &output_parents != required_parent_ids {
                    return Err(ContractError::InvalidContract(
                        "parent output tuples must exactly cover the required parent IDs"
                            .to_owned(),
                    ));
                }
                Ok(())
            }
            (
                RecoveryKindV1::Doctest,
                Some(RecoveryPendingRequirementV1::Doctest {
                    baseline_commit,
                    reasons,
                    required_package_targets,
                }),
            ) if !baseline_commit.is_empty()
                && !reasons.is_empty()
                && !required_package_targets.is_empty()
                && matches!(
                    (&self.legacy_evidence, &self.current_audit),
                    (
                        RecoveryLegacyEvidenceV1::Doctest {
                            historical_raw_count: None,
                            ..
                        },
                        RecoveryCurrentAuditV1::Doctest {
                            raw_count: None,
                            ..
                        }
                    )
                ) =>
            {
                crate::canonical::validate_nfc(baseline_commit)?;
                ensure_sorted_unique(reasons, "doctest recovery reasons")?;
                ensure_sorted_unique(required_package_targets, "required doctest package targets")
            }
            _ => Err(ContractError::InvalidContract(
                "recovery kind and pending requirement disagree".to_owned(),
            )),
        }
    }

    fn validate_resolved(&self) -> Result<(), ContractError> {
        let resolution = self.resolution.as_ref().ok_or_else(|| {
            ContractError::InvalidContract("resolved recovery has no resolution".to_owned())
        })?;
        if resolution.child_sources.is_empty() {
            return Err(ContractError::InvalidContract(
                "resolved recovery must contain recovered child identities".to_owned(),
            ));
        }
        let mut child_ids = Vec::with_capacity(resolution.child_sources.len());
        for child in &resolution.child_sources {
            if child.gap_id != self.gap_id {
                return Err(ContractError::InvalidContract(
                    "recovered child points at a different gap".to_owned(),
                ));
            }
            child_ids.push(child.effective_test_id()?);
        }
        ensure_sorted_unique(&child_ids, "recovered child IDs")?;
        ensure_sorted_unique(&resolution.parent_container_ids, "parent container IDs")?;
        if self.kind == RecoveryKindV1::Unittest {
            if resolution.parent_recapture_outputs.len()
                != UNITTEST_EXECUTABLE_RECAPTURE_PARENT_COUNT
            {
                return Err(ContractError::InvalidContract(
                    "resolved unittest recovery must prove all 893 executable parent outputs"
                        .to_owned(),
                ));
            }
            let parents = resolution
                .parent_recapture_outputs
                .iter()
                .map(|entry| entry.parent_id.clone())
                .collect::<Vec<_>>();
            ensure_sorted_unique(&parents, "resolved parent outputs")?;
            let mut method_body_parents = resolution
                .child_sources
                .iter()
                .filter(|child| child.child_kind == RecoveredChildKindV1::UnittestMethodBody)
                .map(|child| child.parent_baseline_id.clone())
                .collect::<Vec<_>>();
            method_body_parents.sort();
            if resolution.parent_container_ids != parents
                || method_body_parents != parents
                || resolution.child_sources.iter().any(|child| {
                    !matches!(
                        child.child_kind,
                        RecoveredChildKindV1::UnittestMethodBody
                            | RecoveredChildKindV1::UnittestSubtest
                    ) || parents.binary_search(&child.parent_baseline_id).is_err()
                })
            {
                return Err(ContractError::InvalidContract(
                    "resolved unittest recovery must cover exactly the 893 executable parents with one method-body child; hidden-at-freeze ledger identities must not be included"
                        .to_owned(),
                ));
            }
            let mut subtest_occurrences =
                std::collections::BTreeMap::<(&str, &str), Vec<u64>>::new();
            for child in &resolution.child_sources {
                if child.child_kind == RecoveredChildKindV1::UnittestSubtest {
                    subtest_occurrences
                        .entry((
                            child.parent_baseline_id.as_str(),
                            child.declared_site_id.as_deref().unwrap_or_default(),
                        ))
                        .or_default()
                        .push(child.occurrence_ordinal.unwrap_or(u64::MAX));
                }
            }
            if subtest_occurrences.values_mut().any(|ordinals| {
                ordinals.sort_unstable();
                ordinals.iter().copied().ne(0..ordinals.len() as u64)
            }) {
                return Err(ContractError::InvalidContract(
                    "subtest occurrences must be contiguous and zero-based per parent/site"
                        .to_owned(),
                ));
            }
            let recovered_subtest_count = resolution
                .child_sources
                .iter()
                .filter(|child| child.child_kind == RecoveredChildKindV1::UnittestSubtest)
                .count() as u64;
            if !matches!(
                &self.legacy_evidence,
                RecoveryLegacyEvidenceV1::Unittest {
                    historical_subtest_call_count: Some(historical),
                    ..
                } if *historical == recovered_subtest_count
            ) {
                return Err(ContractError::InvalidContract(
                    "resolved unittest historical subtest count must equal every recovered subtest occurrence"
                        .to_owned(),
                ));
            }
        } else {
            if !resolution.parent_recapture_outputs.is_empty()
                || resolution.parent_container_ids.len() != 5
                || resolution.child_sources.iter().any(|child| {
                    child.child_kind != RecoveredChildKindV1::Doctest
                        || resolution
                            .parent_container_ids
                            .binary_search(&child.parent_baseline_id)
                            .is_err()
                })
            {
                return Err(ContractError::InvalidContract(
                    "doctest recovery must replace each of five unique parents with ordinal children"
                        .to_owned(),
                ));
            }
            let mut doctest_ordinals = std::collections::BTreeMap::<&str, Vec<u64>>::new();
            for child in &resolution.child_sources {
                doctest_ordinals
                    .entry(child.parent_baseline_id.as_str())
                    .or_default()
                    .push(child.occurrence_ordinal.unwrap_or(u64::MAX));
            }
            if doctest_ordinals.len() != 5
                || doctest_ordinals.values_mut().any(|ordinals| {
                    ordinals.sort_unstable();
                    ordinals.iter().copied().ne(0..ordinals.len() as u64)
                })
            {
                return Err(ContractError::InvalidContract(
                    "doctest ordinals must be contiguous and zero-based for all five parents"
                        .to_owned(),
                ));
            }
            let recovered_count = resolution.child_sources.len() as u64;
            if !matches!(
                (&self.legacy_evidence, &self.current_audit),
                (
                    RecoveryLegacyEvidenceV1::Doctest {
                        historical_raw_count: Some(historical),
                        ..
                    },
                    RecoveryCurrentAuditV1::Doctest {
                        raw_count: Some(current),
                        ..
                    }
                ) if *historical == recovered_count && *current == recovered_count
            ) {
                return Err(ContractError::InvalidContract(
                    "resolved doctest recovery counts must equal every recovered occurrence"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}

impl InventoryRecoveryAuthorityV1 {
    pub const FORMAT_ID: &'static str = "kd4.inventory-recovery-authority.v1";
    pub const SEMANTIC_HASH_DOMAIN: &'static str = "kd4.inventory-recovery-authority.semantic.v1";
    pub const SELF_HASH_DOMAIN: &'static str = "kd4.inventory-recovery-authority.self.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        if self.schema_version != 1
            || self.format_id != Self::FORMAT_ID
            || self.frozen_source_authority.baseline_commit.is_empty()
            || self.records.len() != 2
        {
            return Err(ContractError::InvalidContract(
                "invalid InventoryRecoveryAuthorityV1 envelope".to_owned(),
            ));
        }
        self.frozen_source_authority.semantic_sha256()?;
        for record in &self.records {
            record.validate()?;
            if let Some(requirement) = &record.pending_requirement {
                let baseline_commit = match requirement {
                    RecoveryPendingRequirementV1::Unittest {
                        baseline_commit, ..
                    }
                    | RecoveryPendingRequirementV1::Doctest {
                        baseline_commit, ..
                    } => baseline_commit,
                };
                if baseline_commit != &self.frozen_source_authority.baseline_commit {
                    return Err(ContractError::InvalidContract(
                        "recovery record and frozen-source baseline commits disagree".to_owned(),
                    ));
                }
            }
        }
        if !matches!(self.records[0].kind, RecoveryKindV1::Doctest)
            || !matches!(self.records[1].kind, RecoveryKindV1::Unittest)
            || self.records[0].recovery_id >= self.records[1].recovery_id
            || self.records[0].gap_id == self.records[1].gap_id
        {
            return Err(ContractError::InvalidContract(
                "recovery records must be the unique sorted doctest and unittest gaps".to_owned(),
            ));
        }
        let semantic_sha256 = proof_hash(
            Self::SEMANTIC_HASH_DOMAIN,
            &InventoryRecoveryAuthoritySemanticProjectionV1 {
                format_id: &self.format_id,
                frozen_source_authority: &self.frozen_source_authority,
                records: &self.records,
                schema_version: self.schema_version,
            },
        )?;
        if self.semantic_sha256 != semantic_sha256 {
            return Err(ContractError::InvalidContract(
                "recovery authority semantic hash mismatch".to_owned(),
            ));
        }
        let self_hash = proof_hash(
            Self::SELF_HASH_DOMAIN,
            &InventoryRecoveryAuthoritySelfProjectionV1 {
                format_id: &self.format_id,
                frozen_source_authority: &self.frozen_source_authority,
                records: &self.records,
                schema_version: self.schema_version,
                semantic_sha256: &self.semantic_sha256,
            },
        )?;
        if self.self_hash != self_hash {
            return Err(ContractError::InvalidContract(
                "recovery authority self hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct InventoryRecoveryAuthoritySemanticProjectionV1<'a> {
    format_id: &'a str,
    frozen_source_authority: &'a FrozenSourceAuthorityV1,
    records: &'a [InventoryRecoveryRecordV1],
    schema_version: u8,
}

#[derive(Serialize)]
struct InventoryRecoveryAuthoritySelfProjectionV1<'a> {
    format_id: &'a str,
    frozen_source_authority: &'a FrozenSourceAuthorityV1,
    records: &'a [InventoryRecoveryRecordV1],
    schema_version: u8,
    semantic_sha256: &'a Sha256HexV1,
}

fn ensure_sorted_unique(values: &[String], label: &str) -> Result<(), ContractError> {
    validate_sorted_unique_nfc_strings(values, label, true)
}

fn ensure_sorted_unique_jcs<T: Serialize>(values: &[T], label: &str) -> Result<(), ContractError> {
    let encoded = values
        .iter()
        .map(crate::canonical::canonical_jcs_of)
        .collect::<Result<Vec<_>, _>>()?;
    if encoded.windows(2).any(|pair| pair[0] >= pair[1]) {
        Err(ContractError::InvalidContract(format!(
            "{label} must be sorted and unique by canonical JCS bytes"
        )))
    } else {
        Ok(())
    }
}
