use crate::canonical::ContractError;
use crate::canonical::Sha256HexV1;
use crate::canonical::proof_hash;
use crate::canonical::validate_nonempty_nfc;
use crate::canonical::validate_sorted_unique_nfc_strings;
use crate::path::StrictRepositoryPathV1;
use crate::runner::TestRouteIdV1;
use crate::selection::ExecutableIdentityV1;
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

pub const DOCTEST_RECAPTURE_BASELINE_COMMIT: &str =
    "60bb133fa0a4f25e83851ab16d8c462e5f42ff95";
pub const DOCTEST_RECAPTURE_SOURCE_TREE_SHA256: &str =
    "654591dd1ddda7a77312172ec7c70e60e80990590c7c74a7c3b08a445279d90e";
pub const DOCTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256: &str =
    "f386e4786f3a61829ecdd61e764fa9d65eddbd08f2902745c6480bce448573cc";
pub const DOCTEST_RECAPTURE_TOOLCHAIN: &str = "1.95.0-x86_64-pc-windows-msvc";

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
            || self.source_isolation.archive_command
                != expected_archive.map(str::to_owned).to_vec()
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
                "cargo", "test", "--locked", "--offline", "-p", package_name, "--doc", "--",
                "--list", "--format", "terse",
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
            let stdout = decode_artifact(
                &run.stdout_base64,
                &run.stdout_sha256,
                "doctest stdout",
            )?;
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
    let raw = STANDARD.decode(encoded).map_err(|_| {
        ContractError::InvalidContract(format!("{label} must be standard base64"))
    })?;
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
            ) if *frozen_parent_count == 909 && historical_subtest_call_count.is_none() => {}
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
            ) if *frozen_unique_count == 5
                && *declared_count == 5
                && *unique_count == 5 => {}
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
                    *line == 0
                        || *column == 0
                        || *path != "scripts/test_rust_test_runner.py"
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
                    || *required_parent_count != 909
                    || required_parent_ids.len() != 909
                    || expected_parent_output_sha256s.len() != 909
                {
                    return Err(ContractError::InvalidContract(
                        "pending unittest recovery must cover all 909 frozen parents".to_owned(),
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
                        RecoveryCurrentAuditV1::Doctest { raw_count: None, .. }
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
            if resolution.parent_recapture_outputs.len() != 909 {
                return Err(ContractError::InvalidContract(
                    "resolved unittest recovery must prove all 909 parent outputs".to_owned(),
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
                    "all 909 unittest parents must become containers with one method-body child"
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
