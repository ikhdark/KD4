use crate::applicability::PlatformApplicabilityV1;
use crate::canonical::ContractError;
use crate::canonical::MustBeNullV1;
use crate::canonical::Sha256HexV1;
use crate::canonical::parse_canonical_jcs;
use crate::canonical::proof_hash;
use crate::canonical::validate_identifier;
use crate::canonical::validate_nfc;
use crate::canonical::validate_nonempty_nfc;
use crate::canonical::validate_sorted_unique_nfc_strings;
use crate::path::StrictRepositoryPathV1;
use crate::runner::RunnerSelectorV1;
use crate::runner::TestRouteIdV1;
use crate::runner::TestRunnerKindV1;
use crate::selection::ActionIdV1;
use crate::selection::ExecutableIdentityV1;
use crate::selection::TestIdV1;
use crate::selection::ValidationIdV1;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

const UNITTEST_V1_LEDGER_PARENT_COUNT: usize = 909;
const UNITTEST_V1_EXECUTABLE_PARENT_COUNT: usize = 893;
const UNITTEST_V1_HIDDEN_PARENT_COUNT: usize = 16;
const UNITTEST_V1_REPLACEMENT_PARENT_COUNT: usize = 536;
const UNITTEST_V1_EXECUTABLE_REPLACEMENT_PARENT_COUNT: usize = 520;
const UNITTEST_V1_UNRESOLVED_PARENT_COUNT: usize = 373;
const UNITTEST_V1_HIDDEN_PARENT_IDS_SHA256: &str =
    "936330f9e9a23c8d628f651a1ed31b3f4ea836a06cf152a6acf09cff898ebc40";
const UNITTEST_V1_REPLACEMENT_PARENT_IDS_SHA256: &str =
    "59202883ef32488ae9a488345b2401400794d961e5d539c58fd6364e86f25fe1";
const UNITTEST_V1_EXECUTABLE_REPLACEMENT_PARENT_IDS_SHA256: &str =
    "208d6735c1413d94c503a453331889fe5709b8eca540cf9c13993c42f9bb8cf7";
const UNITTEST_V1_UNRESOLVED_PARENT_IDS_SHA256: &str =
    "d1e66a89d1a943b60f6516bc9550102306919d6ae673bee4027595a3df8036f7";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryAuthorityV1 {
    pub format_id: String,
    pub raw_sha256: Sha256HexV1,
    pub schema_sha256: Sha256HexV1,
    pub semantic_sha256: Sha256HexV1,
    pub self_hash: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredecessorInventoryV1 {
    pub inventory_hash: Sha256HexV1,
    pub raw_sha256: Sha256HexV1,
    pub recorded_baseline_workspace_fingerprint: Sha256HexV1,
    pub test_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredecessorArtifactReconciliationV1 {
    pub frozen_baseline_associations: Vec<FrozenBaselineAssociationV1>,
    pub frozen_baseline_associations_sha256: Sha256HexV1,
    pub frozen_baseline_ids: Vec<String>,
    pub frozen_baseline_ids_sha256: Sha256HexV1,
    pub frozen_inventory_raw_sha256: Sha256HexV1,
    pub frozen_inventory_semantic_sha256: Sha256HexV1,
    pub frozen_ledger_raw_sha256: Sha256HexV1,
    pub projection_sha256: Sha256HexV1,
    pub schema_version: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenBaselineAssociationV1 {
    pub baseline_id: String,
    pub predecessor_entry_sha256: Sha256HexV1,
}

impl PredecessorArtifactReconciliationV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.predecessor-artifact-reconciliation.v1";
    pub const FROZEN_INVENTORY_RAW_SHA256: &'static str =
        "df230a7683f0f31f1aae4d3f7644af39cec67b09fadf8f3f1e6c60729d18196a";
    pub const FROZEN_INVENTORY_SEMANTIC_SHA256: &'static str =
        "a2fb8c0b806853b6375d92cfa6daf985ea35a5c4d4ecd49cf1d6da6f23359152";
    pub const FROZEN_LEDGER_RAW_SHA256: &'static str =
        "210acb8428be83b9c6acd021bde4e44905e738d557ca89db60cb1271f65da46b";
    pub const FROZEN_BASELINE_IDS_SHA256: &'static str =
        "9100d0fe0dd4c270a1216b6d3ec6f6c39b7f93278cf68235f500edae17d25eb1";
    pub const FROZEN_BASELINE_ASSOCIATIONS_SHA256: &'static str =
        "fe4ee73177d7192ad505e30d20dd851243d981ccf2ab42de49d7d83899624908";

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        if self.schema_version != 1
            || self.frozen_inventory_raw_sha256.as_str() != Self::FROZEN_INVENTORY_RAW_SHA256
            || self.frozen_inventory_semantic_sha256.as_str()
                != Self::FROZEN_INVENTORY_SEMANTIC_SHA256
            || self.frozen_ledger_raw_sha256.as_str() != Self::FROZEN_LEDGER_RAW_SHA256
            || self.frozen_baseline_ids_sha256.as_str() != Self::FROZEN_BASELINE_IDS_SHA256
            || self.frozen_baseline_associations_sha256.as_str()
                != Self::FROZEN_BASELINE_ASSOCIATIONS_SHA256
        {
            return Err(ContractError::InvalidContract(
                "predecessor reconciliation does not bind the exact frozen V1 artifacts".to_owned(),
            ));
        }
        validate_sorted_unique_nfc_strings(&self.frozen_baseline_ids, "frozen baseline IDs", true)?;
        if self.frozen_baseline_ids.len() != 15_544 {
            return Err(ContractError::InvalidContract(
                "predecessor reconciliation requires every one of the 15,544 baseline IDs"
                    .to_owned(),
            ));
        }
        if self.frozen_baseline_associations.len() != 15_544
            || self
                .frozen_baseline_associations
                .windows(2)
                .any(|pair| pair[0].baseline_id >= pair[1].baseline_id)
            || self
                .frozen_baseline_associations
                .iter()
                .map(|association| &association.baseline_id)
                .ne(self.frozen_baseline_ids.iter())
        {
            return Err(ContractError::InvalidContract(
                "frozen baseline associations must exactly bind every predecessor ID".to_owned(),
            ));
        }
        for association in &self.frozen_baseline_associations {
            validate_nonempty_nfc(&association.baseline_id, "frozen baseline association ID")?;
        }
        if proof_hash(
            "kd4.frozen-baseline-association-set.v1",
            &self.frozen_baseline_associations,
        )? != self.frozen_baseline_associations_sha256
        {
            return Err(ContractError::InvalidContract(
                "frozen baseline association set does not match the authenticated predecessor entries"
                    .to_owned(),
            ));
        }
        if proof_hash("kd4.frozen-baseline-id-set.v1", &self.frozen_baseline_ids)?
            != self.frozen_baseline_ids_sha256
        {
            return Err(ContractError::InvalidContract(
                "frozen baseline ID set does not match the authenticated predecessor set"
                    .to_owned(),
            ));
        }
        let expected = proof_hash(
            Self::HASH_DOMAIN,
            &PredecessorArtifactReconciliationHashProjectionV1 {
                frozen_baseline_associations: &self.frozen_baseline_associations,
                frozen_baseline_associations_sha256: &self.frozen_baseline_associations_sha256,
                frozen_baseline_ids: &self.frozen_baseline_ids,
                frozen_baseline_ids_sha256: &self.frozen_baseline_ids_sha256,
                frozen_inventory_raw_sha256: &self.frozen_inventory_raw_sha256,
                frozen_inventory_semantic_sha256: &self.frozen_inventory_semantic_sha256,
                frozen_ledger_raw_sha256: &self.frozen_ledger_raw_sha256,
                schema_version: self.schema_version,
            },
        )?;
        if expected != self.projection_sha256 {
            return Err(ContractError::InvalidContract(
                "predecessor reconciliation projection hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn validate_observed_baseline_ids(
        &self,
        observed: &[String],
        label: &str,
    ) -> Result<(), ContractError> {
        validate_sorted_unique_nfc_strings(observed, label, true)?;
        if observed != self.frozen_baseline_ids {
            return Err(ContractError::InvalidContract(format!(
                "{label} do not exactly reconcile every frozen V1 baseline ID"
            )));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct PredecessorArtifactReconciliationHashProjectionV1<'a> {
    frozen_baseline_associations: &'a [FrozenBaselineAssociationV1],
    frozen_baseline_associations_sha256: &'a Sha256HexV1,
    frozen_baseline_ids: &'a [String],
    frozen_baseline_ids_sha256: &'a Sha256HexV1,
    frozen_inventory_raw_sha256: &'a Sha256HexV1,
    frozen_inventory_semantic_sha256: &'a Sha256HexV1,
    frozen_ledger_raw_sha256: &'a Sha256HexV1,
    schema_version: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaResourceRefV1 {
    pub path: StrictRepositoryPathV1,
    pub raw_sha256: Sha256HexV1,
    pub schema_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestRouteV1 {
    pub route_id: TestRouteIdV1,
    pub runner_kind: TestRunnerKindV1,
    pub validation_id: ValidationIdV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRouteV1 {
    pub action_id: ActionIdV1,
    pub execution_input_contract_sha256: Sha256HexV1,
    pub validation_id: ValidationIdV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CargoTargetKindV1 {
    Lib,
    ProcMacro,
    Bin,
    Example,
    Test,
    Bench,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CargoFeatureSelectionV1 {
    Default { additional_features: Vec<String> },
    NoDefault { features: Vec<String> },
    All,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CargoTargetContextSpecV1 {
    pub cargo_profile: String,
    pub context_sha256: Sha256HexV1,
    pub feature_selection: CargoFeatureSelectionV1,
    pub package_manifest_path: StrictRepositoryPathV1,
    pub package_name: String,
    pub schema_version: u8,
    pub target_kind: CargoTargetKindV1,
    pub target_name: String,
    pub target_source_path: StrictRepositoryPathV1,
    pub workspace_manifest_path: StrictRepositoryPathV1,
}

#[derive(Serialize)]
struct CargoTargetContextHashProjectionV1<'a> {
    cargo_profile: &'a str,
    feature_selection: &'a CargoFeatureSelectionV1,
    package_manifest_path: &'a StrictRepositoryPathV1,
    package_name: &'a str,
    schema_version: u8,
    target_kind: CargoTargetKindV1,
    target_name: &'a str,
    target_source_path: &'a StrictRepositoryPathV1,
    workspace_manifest_path: &'a StrictRepositoryPathV1,
}

impl CargoFeatureSelectionV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        let features = match self {
            Self::Default {
                additional_features,
            } => additional_features,
            Self::NoDefault { features } => features,
            Self::All => return Ok(()),
        };
        if features.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ContractError::InvalidContract(
                "Cargo features must be sorted and unique".to_owned(),
            ));
        }
        for feature in features {
            validate_nonempty_nfc(feature, "Cargo feature")?;
            if !feature.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
                || feature.contains(',')
                || feature.chars().any(char::is_whitespace)
            {
                return Err(ContractError::InvalidContract(
                    "Cargo features must be printable ASCII without commas or whitespace"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }

    pub fn command_arguments(&self) -> Vec<String> {
        match self {
            Self::Default {
                additional_features,
            } if additional_features.is_empty() => vec![],
            Self::Default {
                additional_features,
            } => {
                vec!["--features".to_owned(), additional_features.join(",")]
            }
            Self::NoDefault { features } if features.is_empty() => {
                vec!["--no-default-features".to_owned()]
            }
            Self::NoDefault { features } => vec![
                "--no-default-features".to_owned(),
                "--features".to_owned(),
                features.join(","),
            ],
            Self::All => vec!["--all-features".to_owned()],
        }
    }
}

impl CargoTargetContextSpecV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.cargo-target-context-spec.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        if self.schema_version != 1 || self.cargo_profile != "test" {
            return Err(ContractError::InvalidContract(
                "Cargo target context must use schema version 1 and the test profile".to_owned(),
            ));
        }
        validate_nonempty_nfc(&self.package_name, "Cargo package name")?;
        validate_nonempty_nfc(&self.target_name, "Cargo target name")?;
        self.feature_selection.validate()?;
        if self.semantic_sha256()? != self.context_sha256 {
            return Err(ContractError::InvalidContract(
                "Cargo target context hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn semantic_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        proof_hash(
            Self::HASH_DOMAIN,
            &CargoTargetContextHashProjectionV1 {
                cargo_profile: &self.cargo_profile,
                feature_selection: &self.feature_selection,
                package_manifest_path: &self.package_manifest_path,
                package_name: &self.package_name,
                schema_version: self.schema_version,
                target_kind: self.target_kind,
                target_name: &self.target_name,
                target_source_path: &self.target_source_path,
                workspace_manifest_path: &self.workspace_manifest_path,
            },
        )
    }

    pub fn target_selector_arguments(&self) -> Vec<String> {
        match self.target_kind {
            CargoTargetKindV1::Lib | CargoTargetKindV1::ProcMacro => vec!["--lib".to_owned()],
            CargoTargetKindV1::Bin => vec!["--bin".to_owned(), self.target_name.clone()],
            CargoTargetKindV1::Example => vec!["--example".to_owned(), self.target_name.clone()],
            CargoTargetKindV1::Test => vec!["--test".to_owned(), self.target_name.clone()],
            CargoTargetKindV1::Bench => vec!["--bench".to_owned(), self.target_name.clone()],
        }
    }

    pub fn metadata_command_argv(&self) -> Vec<String> {
        vec![
            "cargo".to_owned(),
            "metadata".to_owned(),
            "--locked".to_owned(),
            "--offline".to_owned(),
            "--format-version".to_owned(),
            "1".to_owned(),
            "--no-deps".to_owned(),
            "--manifest-path".to_owned(),
            self.workspace_manifest_path.as_str().to_owned(),
        ]
    }

    pub fn rustc_cfg_command_argv(
        &self,
        target_triple: &str,
    ) -> Result<Vec<String>, ContractError> {
        self.validate()?;
        validate_nonempty_nfc(target_triple, "Rust target triple")?;
        let mut argv = vec![
            "cargo".to_owned(),
            "rustc".to_owned(),
            "--locked".to_owned(),
            "--offline".to_owned(),
            "--profile".to_owned(),
            "test".to_owned(),
            "--manifest-path".to_owned(),
            "Cargo.toml".to_owned(),
            "--package".to_owned(),
            self.package_name.clone(),
        ];
        argv.extend(self.target_selector_arguments());
        argv.extend(["--target".to_owned(), target_triple.to_owned()]);
        argv.extend(self.feature_selection.command_arguments());
        argv.extend([
            "--".to_owned(),
            "--test".to_owned(),
            "--print".to_owned(),
            "cfg".to_owned(),
        ]);
        Ok(argv)
    }

    pub fn nextest_list_command_argv(
        &self,
        target_triple: &str,
    ) -> Result<Vec<String>, ContractError> {
        self.validate()?;
        let mut argv = vec![
            "cargo".to_owned(),
            "nextest".to_owned(),
            "list".to_owned(),
            "--locked".to_owned(),
            "--offline".to_owned(),
            "--cargo-profile".to_owned(),
            "test".to_owned(),
            "--manifest-path".to_owned(),
            "Cargo.toml".to_owned(),
            "--config-file".to_owned(),
            ".config/nextest.toml".to_owned(),
            "--user-config-file".to_owned(),
            "none".to_owned(),
            "--message-format".to_owned(),
            "json".to_owned(),
            "--ignore-default-filter".to_owned(),
            "--run-ignored".to_owned(),
            "all".to_owned(),
            "--package".to_owned(),
            self.package_name.clone(),
        ];
        argv.extend(self.target_selector_arguments());
        argv.extend(["--target".to_owned(), target_triple.to_owned()]);
        argv.extend(self.feature_selection.command_arguments());
        Ok(argv)
    }

    pub fn doctest_list_command_argv(
        &self,
        target_triple: &str,
    ) -> Result<Vec<String>, ContractError> {
        self.validate()?;
        if !matches!(
            self.target_kind,
            CargoTargetKindV1::Lib | CargoTargetKindV1::ProcMacro
        ) {
            return Err(ContractError::InvalidContract(
                "doctest contexts require a lib or proc-macro target".to_owned(),
            ));
        }
        let mut argv = vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--locked".to_owned(),
            "--offline".to_owned(),
            "--profile".to_owned(),
            "test".to_owned(),
            "--manifest-path".to_owned(),
            "Cargo.toml".to_owned(),
            "--package".to_owned(),
            self.package_name.clone(),
            "--doc".to_owned(),
            "--target".to_owned(),
            target_triple.to_owned(),
        ];
        argv.extend(self.feature_selection.command_arguments());
        argv.extend([
            "--".to_owned(),
            "--list".to_owned(),
            "--format".to_owned(),
            "terse".to_owned(),
        ]);
        Ok(argv)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableInventoryEntryV2 {
    pub cargo_target_context_spec_sha256: Option<Sha256HexV1>,
    pub executable_identity: ExecutableIdentityV1,
    pub executable_identity_sha256: Sha256HexV1,
    pub execution_input_contract_sha256: Sha256HexV1,
    pub platform_applicability: PlatformApplicabilityV1,
    pub platform_applicability_sha256: Sha256HexV1,
    pub runner_selector: RunnerSelectorV1,
    pub runner_selector_sha256: Sha256HexV1,
    pub test_route_id: Option<TestRouteIdV1>,
    pub validation_id: ValidationIdV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedInputLeafV1 {
    pub path: StrictRepositoryPathV1,
    pub provenance: InputLeafProvenanceV1,
    pub raw_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputLeafProvenanceV1 {
    Tracked,
    NonignoredUntracked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedInputLeavesV1 {
    pub contract_input_state_sha256: Sha256HexV1,
    pub execution_input_contract_sha256: Sha256HexV1,
    pub leaves: Vec<ResolvedInputLeafV1>,
    pub matched_path_set_sha256: Sha256HexV1,
    pub raw_sha256: Sha256HexV1,
    pub schema_version: u8,
    pub semantic_inputs_sha256: Sha256HexV1,
    pub semantic_sha256: Sha256HexV1,
    pub self_hash: Sha256HexV1,
    pub workspace_fingerprint: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PathSpecV1 {
    Exact {
        path: StrictRepositoryPathV1,
    },
    Glob {
        pattern: String,
        root: StrictRepositoryPathV1,
    },
    Semantic {
        semantic_input_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionInputContractV1 {
    pub contract_id: String,
    pub contract_sha256: Sha256HexV1,
    pub consumed: Vec<PathSpecV1>,
    pub owned: Vec<PathSpecV1>,
    pub schema_version: u8,
}

impl ExecutionInputContractV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.execution-input-contract.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != 1 || self.owned.is_empty() && self.consumed.is_empty() {
            return Err(ContractError::InvalidContract(
                "execution input contract requires an input".to_owned(),
            ));
        }
        for spec in self.owned.iter().chain(&self.consumed) {
            spec.validate()?;
        }
        ensure_sorted_unique_jcs(&self.owned, "owned path specifications")?;
        ensure_sorted_unique_jcs(&self.consumed, "consumed path specifications")?;
        let projection = ExecutionInputContractHashProjectionV1 {
            consumed: &self.consumed,
            owned: &self.owned,
            schema_version: self.schema_version,
        };
        let expected = proof_hash(Self::HASH_DOMAIN, &projection)?;
        if expected != self.contract_sha256
            || self.contract_id != format!("execution-input-contract-v1.{expected}")
        {
            return Err(ContractError::InvalidContract(
                "execution input contract ID/hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }
}

impl PathSpecV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Exact { .. } => Ok(()),
            Self::Glob { pattern, .. } => {
                validate_nfc(pattern)?;
                if pattern.is_empty()
                    || pattern.starts_with('/')
                    || pattern.contains('\\')
                    || pattern.contains(':')
                    || pattern
                        .split('/')
                        .any(|part| part.is_empty() || matches!(part, "." | ".."))
                {
                    Err(ContractError::InvalidContract(
                        "glob pattern must be nonempty, NFC, repository-relative, and ADS-free"
                            .to_owned(),
                    ))
                } else {
                    Ok(())
                }
            }
            Self::Semantic { semantic_input_id } => validate_identifier(semantic_input_id),
        }
    }
}

impl ResolvedInputLeavesV1 {
    pub const CONTRACT_INPUT_STATE_HASH_DOMAIN: &'static str = "kd4.contract-input-state.v1";
    pub const SEMANTIC_HASH_DOMAIN: &'static str = "kd4.resolved-input-leaves.semantic.v1";
    pub const SELF_HASH_DOMAIN: &'static str = "kd4.resolved-input-leaves.self.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != 1 || self.leaves.is_empty() {
            return Err(ContractError::InvalidContract(
                "resolved input leaves must be version 1 and nonempty".to_owned(),
            ));
        }
        if self
            .leaves
            .windows(2)
            .any(|pair| pair[0].path >= pair[1].path)
        {
            return Err(ContractError::InvalidContract(
                "resolved input leaves must be sorted and path-unique".to_owned(),
            ));
        }
        let semantic = proof_hash(
            Self::SEMANTIC_HASH_DOMAIN,
            &ResolvedInputLeavesSemanticProjectionV1 {
                contract_input_state_sha256: &self.contract_input_state_sha256,
                execution_input_contract_sha256: &self.execution_input_contract_sha256,
                leaves: &self.leaves,
                matched_path_set_sha256: &self.matched_path_set_sha256,
                raw_sha256: &self.raw_sha256,
                schema_version: self.schema_version,
                semantic_inputs_sha256: &self.semantic_inputs_sha256,
                workspace_fingerprint: &self.workspace_fingerprint,
            },
        )?;
        if semantic != self.semantic_sha256 {
            return Err(ContractError::InvalidContract(
                "resolved input leaves semantic hash mismatch".to_owned(),
            ));
        }
        let contract_input_state_sha256 = proof_hash(
            Self::CONTRACT_INPUT_STATE_HASH_DOMAIN,
            &ContractInputStateProjectionV1 {
                execution_input_contract_sha256: &self.execution_input_contract_sha256,
                leaves: &self.leaves,
                matched_path_set_sha256: &self.matched_path_set_sha256,
                semantic_inputs_sha256: &self.semantic_inputs_sha256,
            },
        )?;
        if contract_input_state_sha256 != self.contract_input_state_sha256 {
            return Err(ContractError::InvalidContract(
                "resolved input leaves contract-scoped state hash mismatch".to_owned(),
            ));
        }
        let self_hash = proof_hash(
            Self::SELF_HASH_DOMAIN,
            &ResolvedInputLeavesSelfProjectionV1 {
                contract_input_state_sha256: &self.contract_input_state_sha256,
                execution_input_contract_sha256: &self.execution_input_contract_sha256,
                leaves: &self.leaves,
                matched_path_set_sha256: &self.matched_path_set_sha256,
                raw_sha256: &self.raw_sha256,
                schema_version: self.schema_version,
                semantic_inputs_sha256: &self.semantic_inputs_sha256,
                semantic_sha256: &self.semantic_sha256,
                workspace_fingerprint: &self.workspace_fingerprint,
            },
        )?;
        if self_hash != self.self_hash {
            return Err(ContractError::InvalidContract(
                "resolved input leaves self hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ResolvedInputLeavesSemanticProjectionV1<'a> {
    contract_input_state_sha256: &'a Sha256HexV1,
    execution_input_contract_sha256: &'a Sha256HexV1,
    leaves: &'a [ResolvedInputLeafV1],
    matched_path_set_sha256: &'a Sha256HexV1,
    raw_sha256: &'a Sha256HexV1,
    schema_version: u8,
    semantic_inputs_sha256: &'a Sha256HexV1,
    workspace_fingerprint: &'a Sha256HexV1,
}

#[derive(Serialize)]
struct ResolvedInputLeavesSelfProjectionV1<'a> {
    contract_input_state_sha256: &'a Sha256HexV1,
    execution_input_contract_sha256: &'a Sha256HexV1,
    leaves: &'a [ResolvedInputLeafV1],
    matched_path_set_sha256: &'a Sha256HexV1,
    raw_sha256: &'a Sha256HexV1,
    schema_version: u8,
    semantic_inputs_sha256: &'a Sha256HexV1,
    semantic_sha256: &'a Sha256HexV1,
    workspace_fingerprint: &'a Sha256HexV1,
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

#[derive(Serialize)]
struct ExecutionInputContractHashProjectionV1<'a> {
    consumed: &'a [PathSpecV1],
    owned: &'a [PathSpecV1],
    schema_version: u8,
}

#[derive(Serialize)]
struct ContractInputStateProjectionV1<'a> {
    execution_input_contract_sha256: &'a Sha256HexV1,
    leaves: &'a [ResolvedInputLeafV1],
    matched_path_set_sha256: &'a Sha256HexV1,
    semantic_inputs_sha256: &'a Sha256HexV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExceptionTagV1 {
    Protected,
    Generated,
    LiveService,
    OffHost,
    PlatformPending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvenanceKindV1 {
    HelperDriven,
    Generated,
    Protected,
    LiveService,
    OffHost,
    PlatformPending,
    SourceDeclaration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceReceiptV1 {
    pub evidence_paths: Vec<StrictRepositoryPathV1>,
    pub evidence_sha256: Sha256HexV1,
    pub kind: ProvenanceKindV1,
    pub receipt_sha256: Sha256HexV1,
    pub schema_version: u8,
}

impl ProvenanceReceiptV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.provenance-receipt.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != 1
            || self.evidence_paths.is_empty()
            || self
                .evidence_paths
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(ContractError::InvalidContract(
                "provenance receipt requires version 1 and sorted unique evidence paths".to_owned(),
            ));
        }
        let expected = proof_hash(
            Self::HASH_DOMAIN,
            &ProvenanceReceiptHashProjectionV1 {
                evidence_paths: &self.evidence_paths,
                evidence_sha256: &self.evidence_sha256,
                kind: self.kind,
                schema_version: self.schema_version,
            },
        )?;
        if expected != self.receipt_sha256 {
            return Err(ContractError::InvalidContract(
                "provenance receipt hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ProvenanceReceiptHashProjectionV1<'a> {
    evidence_paths: &'a [StrictRepositoryPathV1],
    evidence_sha256: &'a Sha256HexV1,
    kind: ProvenanceKindV1,
    schema_version: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyReplacementHintV1 {
    pub predecessor_row_sha256: Sha256HexV1,
    pub replacement_ids: Vec<String>,
}

impl LegacyReplacementHintV1 {
    fn validate(&self) -> Result<(), ContractError> {
        validate_sorted_unique_nfc_strings(&self.replacement_ids, "legacy replacement IDs", true)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateReplacementContractV1 {
    pub candidate_receipt_sha256: Sha256HexV1,
    pub executable_identity: ExecutableIdentityV1,
    pub executable_identity_sha256: Sha256HexV1,
    pub execution_input_contract_sha256: Sha256HexV1,
    pub platform_applicability_sha256: Sha256HexV1,
    pub replacement_id: String,
    pub runner_selector: RunnerSelectorV1,
    pub runner_selector_sha256: Sha256HexV1,
    pub test_route_id: TestRouteIdV1,
    pub validation_id: ValidationIdV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedReplacementContractV1 {
    pub accepted_receipt_sha256: Sha256HexV1,
    pub candidate: CandidateReplacementContractV1,
    pub contract_sources_sha256: Sha256HexV1,
    pub product_behavior_obligation_sha256: Sha256HexV1,
    pub resolved_entry_set_sha256: Option<Sha256HexV1>,
    pub runtime_path_sha256: Sha256HexV1,
    pub selection_v1_sha256: Option<Sha256HexV1>,
    pub trusted_defect_receipt_sha256s: Option<Vec<Sha256HexV1>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ReplacementContractDispositionV1 {
    PendingReview {
        accepted: MustBeNullV1,
        candidate: MustBeNullV1,
        legacy_replacement_hint: LegacyReplacementHintV1,
    },
    FocusedCandidate {
        accepted: MustBeNullV1,
        candidate: CandidateReplacementContractV1,
        legacy_replacement_hint: LegacyReplacementHintV1,
    },
    Accepted {
        accepted: AcceptedReplacementContractV1,
        candidate: MustBeNullV1,
        legacy_replacement_hint: LegacyReplacementHintV1,
    },
}

impl ReplacementContractDispositionV1 {
    fn legacy_replacement_hint(&self) -> &LegacyReplacementHintV1 {
        match self {
            Self::PendingReview {
                legacy_replacement_hint,
                ..
            }
            | Self::FocusedCandidate {
                legacy_replacement_hint,
                ..
            }
            | Self::Accepted {
                legacy_replacement_hint,
                ..
            } => legacy_replacement_hint,
        }
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        let legacy_hint = self.legacy_replacement_hint();
        legacy_hint.validate()?;
        let (candidate, selector) = match self {
            Self::PendingReview { .. } => return Ok(()),
            Self::FocusedCandidate { candidate, .. } => (candidate, &candidate.runner_selector),
            Self::Accepted { accepted, .. } => {
                (&accepted.candidate, &accepted.candidate.runner_selector)
            }
        };
        selector.validate()?;
        validate_nfc(&candidate.replacement_id)?;
        if candidate.replacement_id.is_empty() {
            return Err(ContractError::InvalidContract(
                "replacement ID must be nonempty".to_owned(),
            ));
        }
        crate::selection::validate_identity_selector_binding(
            &candidate.executable_identity,
            &candidate.executable_identity_sha256,
            &candidate.runner_selector,
            &candidate.runner_selector_sha256,
        )?;
        match &candidate.executable_identity {
            ExecutableIdentityV1::Test {
                route_id,
                validation_id,
                ..
            } if *route_id == candidate.test_route_id
                && validation_id == &candidate.validation_id =>
            {
                Ok(())
            }
            _ => Err(ContractError::InvalidContract(
                "replacement candidate identity, route, and validation disagree".to_owned(),
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Stage2IncorrectBehaviorIdsV1 {
    Unreviewed(MustBeNullV1),
    Reviewed(Vec<String>),
}

impl Stage2IncorrectBehaviorIdsV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        let Self::Reviewed(ids) = self else {
            return Ok(());
        };
        for id in ids {
            if id.is_empty() {
                return Err(ContractError::InvalidContract(
                    "Stage2 incorrect behavior ID must be nonempty".to_owned(),
                ));
            }
            validate_nfc(id)?;
        }
        if ids.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ContractError::InvalidContract(
                "Stage2 incorrect behavior IDs must be sorted and unique".to_owned(),
            ));
        }
        Ok(())
    }

    fn reviewed_ids(&self) -> Option<&[String]> {
        match self {
            Self::Unreviewed(_) => None,
            Self::Reviewed(ids) => Some(ids),
        }
    }
}

pub fn validate_stage2_incorrect_behavior_ids(
    value: &Stage2IncorrectBehaviorIdsV1,
) -> Result<(), ContractError> {
    value.validate()
}

#[derive(Serialize)]
struct InventoryDeclarationIdProjectionV2<'a> {
    entry: &'a ExecutableInventoryEntryV2,
    kind: &'a str,
    source_provenance: &'a ProvenanceReceiptV1,
}

#[derive(Serialize)]
struct InventoryDeclarationObligationIdProjectionV2<'a> {
    declaration_id: &'a str,
    kind: &'a str,
}

fn inventory_declaration_digest(
    domain: &str,
    kind: &str,
    entry: &ExecutableInventoryEntryV2,
    source_provenance: &ProvenanceReceiptV1,
) -> Result<Sha256HexV1, ContractError> {
    proof_hash(
        domain,
        &InventoryDeclarationIdProjectionV2 {
            entry,
            kind,
            source_provenance,
        },
    )
}

fn inventory_declaration_obligation_digest(
    declaration_id: &str,
    kind: &str,
) -> Result<Sha256HexV1, ContractError> {
    proof_hash(
        "kd4.inventory-declaration-obligation-id.v2",
        &InventoryDeclarationObligationIdProjectionV2 {
            declaration_id,
            kind,
        },
    )
}

pub fn post_baseline_declaration_id(
    entry: &ExecutableInventoryEntryV2,
    source_provenance: &ProvenanceReceiptV1,
) -> Result<String, ContractError> {
    let digest = inventory_declaration_digest(
        "kd4.post-baseline-declaration-id.v2",
        "post-baseline-current",
        entry,
        source_provenance,
    )?;
    Ok(format!("post-baseline-declaration-v2.{digest}"))
}

pub fn post_baseline_obligation_id(
    entry: &ExecutableInventoryEntryV2,
    source_provenance: &ProvenanceReceiptV1,
) -> Result<String, ContractError> {
    let declaration_id = post_baseline_declaration_id(entry, source_provenance)?;
    let digest = inventory_declaration_obligation_digest(&declaration_id, "post-baseline-current")?;
    Ok(format!("inventory-obligation-v2.post-baseline.{digest}"))
}

pub fn missing_baseline_declaration_id(
    entry: &ExecutableInventoryEntryV2,
    source_provenance: &ProvenanceReceiptV1,
) -> Result<String, ContractError> {
    let digest = inventory_declaration_digest(
        "kd4.missing-baseline-declaration-id.v2",
        "missing-baseline",
        entry,
        source_provenance,
    )?;
    Ok(format!("missing-baseline-declaration-v2.{digest}"))
}

pub fn missing_baseline_obligation_id(
    entry: &ExecutableInventoryEntryV2,
    source_provenance: &ProvenanceReceiptV1,
) -> Result<String, ContractError> {
    let declaration_id = missing_baseline_declaration_id(entry, source_provenance)?;
    let digest = inventory_declaration_obligation_digest(&declaration_id, "missing-baseline")?;
    Ok(format!(
        "inventory-obligation-v2.missing-declaration.{digest}"
    ))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum InventoryDeclarationV2 {
    FrozenBaseline {
        baseline_id: String,
        entry: ExecutableInventoryEntryV2,
        predecessor_entry_sha256: Sha256HexV1,
    },
    MissingBaseline {
        declaration_id: String,
        entry: ExecutableInventoryEntryV2,
        obligation_id: String,
        source_provenance: ProvenanceReceiptV1,
    },
    PostBaselineCurrent {
        declaration_id: String,
        entry: ExecutableInventoryEntryV2,
        obligation_id: String,
        source_provenance: ProvenanceReceiptV1,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenTestInventoryV2 {
    pub action_routes: Vec<ActionRouteV1>,
    pub authority: InventoryAuthorityV1,
    pub cargo_target_context_specs: Vec<CargoTargetContextSpecV1>,
    pub declaration_universe: Vec<InventoryDeclarationV2>,
    pub execution_input_contracts: Vec<ExecutionInputContractV1>,
    pub format_id: String,
    pub predecessor: PredecessorInventoryV1,
    pub predecessor_reconciliation: PredecessorArtifactReconciliationV1,
    pub recovery_authority: crate::selection::InventoryAuthorityRefV1,
    pub routes: Vec<TestRouteV1>,
    pub schema_resources: Vec<SchemaResourceRefV1>,
    pub schema_resources_sha256: Sha256HexV1,
    pub schema_version: u8,
}

impl FrozenTestInventoryV2 {
    pub const FORMAT_ID: &'static str = "kd4-frozen-test-inventory-v2";
    pub const SEMANTIC_HASH_DOMAIN: &'static str = "kd4.frozen-test-inventory-v2.semantic";
    pub const AUTHORITY_SELF_HASH_DOMAIN: &'static str =
        "kd4.frozen-test-inventory-v2.authority.self.v1";
    pub const SCHEMA_RESOURCE_SET_HASH_DOMAIN: &'static str =
        "kd4.inventory-v2-schema-resource-set.v1";
    pub const FROZEN_V1_WORKSPACE_FINGERPRINT: &'static str =
        "7d5c019e4af3720e1188704b05a098e95fdb200b901cbccf5cf13363643f34ca";

    pub fn validate_schema_resources(
        resources: &[SchemaResourceRefV1],
        resources_sha256: &Sha256HexV1,
    ) -> Result<(), ContractError> {
        const EXPECTED: [(&str, &str, &str); 5] = [
            (
                ".codex/validation/frozen-test-inventory-v2-recoveries.schema.json",
                "8412a18a13ae6f93b8efd18552e0be1422e957ebde5bde24512e8ce87d8bc09f",
                "kd4://validation/frozen-test-inventory-v2-recoveries.schema.json",
            ),
            (
                ".codex/validation/frozen-test-inventory-v2.schema.json",
                "06d7134aa77d6fa63fea9e7a0094fa76f4c2cca74c6ddad4a55078a09909732a",
                "kd4://validation/frozen-test-inventory-v2.schema.json",
            ),
            (
                ".codex/validation/inventory-shared-types-v1.schema.json",
                "1753d9bbca80f85ace51f9622e4cf228e91e0dfae94f82691a9a4935b2482c19",
                "kd4://validation/inventory-shared-types-v1.schema.json",
            ),
            (
                ".codex/validation/selection-request-v1.schema.json",
                "b83d9fd5a535d358d3da6a9b06652dfc9390877813b5b5f85e01fd56b90e9c78",
                "kd4://validation/selection-request-v1.schema.json",
            ),
            (
                ".codex/validation/test-replacements-v2.schema.json",
                "4a20813531b293ca3a25354aa095c5e9f417aa2169a00f8cbe3edd1442e0f2e6",
                "kd4://validation/test-replacements-v2.schema.json",
            ),
        ];
        if resources.len() != EXPECTED.len() {
            return Err(ContractError::InvalidContract(
                "FrozenTestInventoryV2 must bind exactly five schema resources".to_owned(),
            ));
        }
        ensure_sorted_unique_jcs(resources, "schema resources")?;
        for (resource, (expected_path, expected_raw_sha256, expected_schema_id)) in
            resources.iter().zip(EXPECTED)
        {
            if resource.path.as_str() != expected_path
                || resource.raw_sha256.as_str() != expected_raw_sha256
                || resource.schema_id != expected_schema_id
            {
                return Err(ContractError::InvalidContract(
                    "FrozenTestInventoryV2 schema resource path/raw hash/schema ID claim mismatch"
                        .to_owned(),
                ));
            }
        }
        if proof_hash(Self::SCHEMA_RESOURCE_SET_HASH_DOMAIN, resources)? != *resources_sha256 {
            return Err(ContractError::InvalidContract(
                "inventory schema resource set hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        if self.schema_version != 2
            || self.format_id != Self::FORMAT_ID
            || self.authority.format_id != Self::FORMAT_ID
            || self.action_routes.len() != 2
            || self.execution_input_contracts.is_empty()
            || self.predecessor.test_count != 15_544
            || self.routes.len() != 7
            || self.schema_resources.len() != 5
        {
            return Err(ContractError::InvalidContract(
                "invalid FrozenTestInventoryV2 envelope".to_owned(),
            ));
        }
        if self.predecessor.inventory_hash.as_str()
            != PredecessorArtifactReconciliationV1::FROZEN_INVENTORY_SEMANTIC_SHA256
            || self.predecessor.raw_sha256.as_str()
                != PredecessorArtifactReconciliationV1::FROZEN_INVENTORY_RAW_SHA256
            || self
                .predecessor
                .recorded_baseline_workspace_fingerprint
                .as_str()
                != Self::FROZEN_V1_WORKSPACE_FINGERPRINT
        {
            return Err(ContractError::InvalidContract(
                "FrozenTestInventoryV2 predecessor metadata does not match frozen V1".to_owned(),
            ));
        }
        let expected_routes = [
            TestRouteIdV1::ArgumentCommentLintNative,
            TestRouteIdV1::JavascriptJest,
            TestRouteIdV1::PythonPytest,
            TestRouteIdV1::PythonUnittest,
            TestRouteIdV1::RustDoctest,
            TestRouteIdV1::RustNextest,
            TestRouteIdV1::WindowsSandboxSmokeNative,
        ];
        let actual_routes = self
            .routes
            .iter()
            .map(|route| route.route_id)
            .collect::<Vec<_>>();
        if actual_routes != expected_routes {
            return Err(ContractError::InvalidContract(
                "FrozenTestInventoryV2 must contain each of the seven routes exactly once in canonical order"
                    .to_owned(),
            ));
        }
        let expected_action_ids = ["documentation.markdown", "maintenance.source-map"];
        let actual_action_ids = self
            .action_routes
            .iter()
            .map(|route| route.action_id.as_str())
            .collect::<Vec<_>>();
        if actual_action_ids != expected_action_ids {
            return Err(ContractError::InvalidContract(
                "FrozenTestInventoryV2 must contain documentation.markdown and maintenance.source-map exactly once in canonical order"
                    .to_owned(),
            ));
        }
        for route in &self.routes {
            let expected_runner = match route.route_id {
                TestRouteIdV1::RustNextest => TestRunnerKindV1::RustNextest,
                TestRouteIdV1::RustDoctest => TestRunnerKindV1::RustDoctest,
                TestRouteIdV1::PythonUnittest => TestRunnerKindV1::PythonUnittest,
                TestRouteIdV1::PythonPytest => TestRunnerKindV1::PythonPytest,
                TestRouteIdV1::JavascriptJest => TestRunnerKindV1::JavascriptJest,
                TestRouteIdV1::ArgumentCommentLintNative => {
                    TestRunnerKindV1::ArgumentCommentLintNative
                }
                TestRouteIdV1::WindowsSandboxSmokeNative => {
                    TestRunnerKindV1::WindowsSandboxSmokeNative
                }
            };
            if route.runner_kind != expected_runner {
                return Err(ContractError::InvalidContract(
                    "test route and runner kind disagree".to_owned(),
                ));
            }
        }
        ensure_sorted_unique_jcs(&self.action_routes, "action routes")?;
        self.predecessor_reconciliation.validate()?;
        if self.action_routes[0].execution_input_contract_sha256
            == self.action_routes[1].execution_input_contract_sha256
        {
            return Err(ContractError::InvalidContract(
                "each action route requires its own execution input contract".to_owned(),
            ));
        }
        ensure_sorted_unique_jcs(&self.execution_input_contracts, "execution input contracts")?;
        for contract in &self.execution_input_contracts {
            contract.validate()?;
        }
        if self
            .cargo_target_context_specs
            .windows(2)
            .any(|pair| pair[0].context_sha256 >= pair[1].context_sha256)
        {
            return Err(ContractError::InvalidContract(
                "Cargo target context specs must be sorted uniquely by context hash".to_owned(),
            ));
        }
        for context in &self.cargo_target_context_specs {
            context.validate()?;
        }
        let require_input_contract = |digest: &Sha256HexV1| {
            if self
                .execution_input_contracts
                .iter()
                .filter(|contract| &contract.contract_sha256 == digest)
                .count()
                == 1
            {
                Ok(())
            } else {
                Err(ContractError::InvalidContract(
                    "every executable identity must resolve to exactly one authenticated execution input contract"
                        .to_owned(),
                ))
            }
        };
        for route in &self.action_routes {
            require_input_contract(&route.execution_input_contract_sha256)?;
        }
        Self::validate_schema_resources(&self.schema_resources, &self.schema_resources_sha256)?;
        for declaration in &self.declaration_universe {
            declaration.validate()?;
            let entry = declaration.entry();
            require_input_contract(&entry.execution_input_contract_sha256)?;
            if let Some(context_sha256) = &entry.cargo_target_context_spec_sha256 {
                if self
                    .cargo_target_context_specs
                    .iter()
                    .filter(|context| &context.context_sha256 == context_sha256)
                    .count()
                    != 1
                {
                    return Err(ContractError::InvalidContract(
                        "Rust entries must resolve to exactly one Cargo target context".to_owned(),
                    ));
                }
                let selector_context = match &entry.runner_selector {
                    RunnerSelectorV1::RustNextest {
                        cargo_target_context_spec_sha256,
                        ..
                    }
                    | RunnerSelectorV1::RustDoctest {
                        cargo_target_context_spec_sha256,
                        ..
                    } => Some(cargo_target_context_spec_sha256),
                    _ => None,
                };
                if selector_context != Some(context_sha256) {
                    return Err(ContractError::InvalidContract(
                        "Rust selector and inventory entry Cargo context hashes disagree"
                            .to_owned(),
                    ));
                }
                if matches!(entry.runner_selector, RunnerSelectorV1::RustDoctest { .. }) {
                    let context = self
                        .cargo_target_context_specs
                        .iter()
                        .find(|context| &context.context_sha256 == context_sha256)
                        .expect("exactly one context was established");
                    if !matches!(
                        context.target_kind,
                        CargoTargetKindV1::Lib | CargoTargetKindV1::ProcMacro
                    ) {
                        return Err(ContractError::InvalidContract(
                            "Rust doctest entries require a lib or proc-macro context".to_owned(),
                        ));
                    }
                }
            }
            match (&entry.executable_identity, entry.test_route_id) {
                (
                    ExecutableIdentityV1::Action {
                        action_id,
                        validation_id,
                    },
                    None,
                ) => {
                    let matching = self.action_routes.iter().filter(|route| {
                        &route.action_id == action_id
                            && &route.validation_id == validation_id
                            && route.validation_id == entry.validation_id
                            && route.execution_input_contract_sha256
                                == entry.execution_input_contract_sha256
                    });
                    if matching.count() != 1 {
                        return Err(ContractError::InvalidContract(
                            "action declaration must map to exactly one action route".to_owned(),
                        ));
                    }
                }
                (
                    ExecutableIdentityV1::Test {
                        route_id: identity_route_id,
                        validation_id,
                        ..
                    },
                    Some(route_id),
                ) if *identity_route_id == route_id && validation_id == &entry.validation_id => {
                    let matching = self.routes.iter().filter(|route| {
                        route.route_id == route_id && route.validation_id == entry.validation_id
                    });
                    if matching.count() != 1 {
                        return Err(ContractError::InvalidContract(
                            "test declaration must map to exactly one validation route".to_owned(),
                        ));
                    }
                }
                _ => unreachable!("entry validation rejects this identity/route shape"),
            }
        }
        ensure_sorted_unique_jcs(&self.declaration_universe, "inventory declarations")?;
        let mut observed_baseline_ids = self
            .declaration_universe
            .iter()
            .filter_map(|declaration| match declaration {
                InventoryDeclarationV2::FrozenBaseline { baseline_id, .. } => {
                    Some(baseline_id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        observed_baseline_ids.sort();
        self.predecessor_reconciliation
            .validate_observed_baseline_ids(
                &observed_baseline_ids,
                "inventory frozen-baseline declarations",
            )?;
        for declaration in &self.declaration_universe {
            if let InventoryDeclarationV2::FrozenBaseline {
                baseline_id,
                predecessor_entry_sha256,
                ..
            } = declaration
            {
                let expected = self
                    .predecessor_reconciliation
                    .frozen_baseline_associations
                    .binary_search_by(|association| association.baseline_id.cmp(baseline_id))
                    .ok()
                    .map(|index| {
                        &self.predecessor_reconciliation.frozen_baseline_associations[index]
                            .predecessor_entry_sha256
                    });
                if expected != Some(predecessor_entry_sha256) {
                    return Err(ContractError::InvalidContract(
                        "frozen baseline declaration substituted its predecessor entry association"
                            .to_owned(),
                    ));
                }
            }
        }
        let identities = self
            .declaration_universe
            .iter()
            .map(|declaration| {
                crate::canonical::canonical_jcs_of(&declaration.entry().executable_identity)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut unique_identities = identities.clone();
        unique_identities.sort();
        unique_identities.dedup();
        if unique_identities.len() != identities.len() {
            return Err(ContractError::InvalidContract(
                "inventory declaration executable identities must be unique".to_owned(),
            ));
        }
        let semantic_sha256 = proof_hash(
            Self::SEMANTIC_HASH_DOMAIN,
            &FrozenTestInventorySemanticProjectionV2 {
                action_routes: &self.action_routes,
                cargo_target_context_specs: &self.cargo_target_context_specs,
                declaration_universe: &self.declaration_universe,
                execution_input_contracts: &self.execution_input_contracts,
                format_id: &self.format_id,
                predecessor: &self.predecessor,
                predecessor_reconciliation: &self.predecessor_reconciliation,
                recovery_authority: &self.recovery_authority,
                routes: &self.routes,
                schema_resources: &self.schema_resources,
                schema_resources_sha256: &self.schema_resources_sha256,
                schema_version: self.schema_version,
            },
        )?;
        if semantic_sha256 != self.authority.semantic_sha256 {
            return Err(ContractError::InvalidContract(
                "inventory authority semantic hash mismatch".to_owned(),
            ));
        }
        let self_hash = proof_hash(
            Self::AUTHORITY_SELF_HASH_DOMAIN,
            &InventoryAuthoritySelfProjectionV1 {
                format_id: &self.authority.format_id,
                raw_sha256: &self.authority.raw_sha256,
                schema_sha256: &self.authority.schema_sha256,
                semantic_sha256: &self.authority.semantic_sha256,
            },
        )?;
        if self_hash != self.authority.self_hash {
            return Err(ContractError::InvalidContract(
                "inventory authority self hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct FrozenTestInventorySemanticProjectionV2<'a> {
    action_routes: &'a [ActionRouteV1],
    cargo_target_context_specs: &'a [CargoTargetContextSpecV1],
    declaration_universe: &'a [InventoryDeclarationV2],
    execution_input_contracts: &'a [ExecutionInputContractV1],
    format_id: &'a str,
    predecessor: &'a PredecessorInventoryV1,
    predecessor_reconciliation: &'a PredecessorArtifactReconciliationV1,
    recovery_authority: &'a crate::selection::InventoryAuthorityRefV1,
    routes: &'a [TestRouteV1],
    schema_resources: &'a [SchemaResourceRefV1],
    schema_resources_sha256: &'a Sha256HexV1,
    schema_version: u8,
}

#[derive(Serialize)]
struct InventoryAuthoritySelfProjectionV1<'a> {
    format_id: &'a str,
    raw_sha256: &'a Sha256HexV1,
    schema_sha256: &'a Sha256HexV1,
    semantic_sha256: &'a Sha256HexV1,
}

impl InventoryDeclarationV2 {
    pub fn entry(&self) -> &ExecutableInventoryEntryV2 {
        match self {
            Self::FrozenBaseline { entry, .. }
            | Self::MissingBaseline { entry, .. }
            | Self::PostBaselineCurrent { entry, .. } => entry,
        }
    }

    pub fn obligation_id(&self) -> Result<String, ContractError> {
        match self {
            Self::FrozenBaseline { .. } => frozen_baseline_obligation_id(self),
            Self::MissingBaseline { obligation_id, .. }
            | Self::PostBaselineCurrent { obligation_id, .. } => Ok(obligation_id.clone()),
        }
    }

    pub fn baseline_id(&self) -> Option<&str> {
        match self {
            Self::FrozenBaseline { baseline_id, .. } => Some(baseline_id),
            Self::MissingBaseline { .. } | Self::PostBaselineCurrent { .. } => None,
        }
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::FrozenBaseline {
                baseline_id, entry, ..
            } => {
                if baseline_id.is_empty() {
                    return Err(ContractError::InvalidContract(
                        "frozen baseline ID must be nonempty".to_owned(),
                    ));
                }
                validate_nfc(baseline_id)?;
                entry.validate()
            }
            Self::MissingBaseline {
                declaration_id,
                entry,
                obligation_id,
                source_provenance,
            } => {
                entry.validate()?;
                source_provenance.validate()?;
                if *declaration_id != missing_baseline_declaration_id(entry, source_provenance)?
                    || *obligation_id != missing_baseline_obligation_id(entry, source_provenance)?
                {
                    return Err(ContractError::InvalidContract(
                        "missing-baseline declaration IDs do not bind the exact entry and typed provenance"
                            .to_owned(),
                    ));
                }
                Ok(())
            }
            Self::PostBaselineCurrent {
                declaration_id,
                entry,
                obligation_id,
                source_provenance,
            } => {
                entry.validate()?;
                source_provenance.validate()?;
                if *declaration_id != post_baseline_declaration_id(entry, source_provenance)?
                    || *obligation_id != post_baseline_obligation_id(entry, source_provenance)?
                {
                    return Err(ContractError::InvalidContract(
                        "post-baseline declaration IDs do not bind the exact entry and typed provenance"
                            .to_owned(),
                    ));
                }
                Ok(())
            }
        }
    }
}

pub fn frozen_baseline_obligation_id(
    declaration: &InventoryDeclarationV2,
) -> Result<String, ContractError> {
    if !matches!(declaration, InventoryDeclarationV2::FrozenBaseline { .. }) {
        return Err(ContractError::InvalidContract(
            "frozen obligation ID requires a frozen declaration".to_owned(),
        ));
    }
    let digest = proof_hash("kd4.frozen-baseline-obligation-id.v2", declaration)?;
    Ok(format!("inventory-obligation-v2.frozen-baseline.{digest}"))
}

impl ExecutableInventoryEntryV2 {
    pub const SEMANTIC_HASH_DOMAIN: &'static str = "kd4.executable-inventory-entry.v2";

    pub fn semantic_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        self.validate()?;
        proof_hash(Self::SEMANTIC_HASH_DOMAIN, self)
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        self.platform_applicability.validate()?;
        if self.platform_applicability.semantic_sha256()? != self.platform_applicability_sha256 {
            return Err(ContractError::InvalidContract(
                "inventory entry applicability hash mismatch".to_owned(),
            ));
        }
        self.runner_selector.validate()?;
        crate::selection::validate_identity_selector_binding(
            &self.executable_identity,
            &self.executable_identity_sha256,
            &self.runner_selector,
            &self.runner_selector_sha256,
        )?;
        match (&self.executable_identity, self.test_route_id) {
            (ExecutableIdentityV1::Action { .. }, None) => {}
            (ExecutableIdentityV1::Action { .. }, Some(_)) => {
                return Err(ContractError::InvalidContract(
                    "action inventory entry cannot have a test route".to_owned(),
                ));
            }
            (_, None) => {
                return Err(ContractError::InvalidContract(
                    "test inventory entry requires a test route".to_owned(),
                ));
            }
            _ => {}
        }
        let rust_identity = matches!(
            self.executable_identity,
            ExecutableIdentityV1::Test {
                route_id: TestRouteIdV1::RustNextest | TestRouteIdV1::RustDoctest,
                ..
            }
        );
        if rust_identity != self.cargo_target_context_spec_sha256.is_some() {
            return Err(ContractError::InvalidContract(
                "Cargo target context is required exactly for Rust test entries".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ExceptionDispositionV1 {
    PendingLegacy {
        provenance_receipt: ProvenanceReceiptV1,
        tag: ExceptionTagV1,
    },
    Accepted {
        active_host_authority: crate::applicability::ActiveHostApplicabilityAuthorityV1,
        provenance_receipt: ProvenanceReceiptV1,
        receipt_sha256: Sha256HexV1,
        tag: ExceptionTagV1,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ReplacementLedgerDispositionV2 {
    Unresolved,
    Current {
        inventory_entry_semantic_sha256: Sha256HexV1,
    },
    Replacement {
        contract: ReplacementContractDispositionV1,
        edge_ids: Vec<String>,
        stage2_incorrect_behavior_ids: Stage2IncorrectBehaviorIdsV1,
    },
    Exception {
        exception: ExceptionDispositionV1,
    },
    RecoveredContainer {
        child_obligation_ids: Vec<String>,
        transition_receipt_sha256: Sha256HexV1,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementLedgerRowV2 {
    pub baseline_id: Option<String>,
    pub disposition: ReplacementLedgerDispositionV2,
    pub obligation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestReplacementLedgerV2 {
    pub format_id: String,
    pub inventory_authority: crate::selection::InventoryAuthorityRefV1,
    pub rows: Vec<ReplacementLedgerRowV2>,
    pub schema_version: u8,
    pub semantic_sha256: Sha256HexV1,
    pub self_hash: Sha256HexV1,
    pub trusted_defect_receipts: Option<Vec<crate::receipts::TrustedDefectReceiptV1>>,
}

impl TestReplacementLedgerV2 {
    pub const FORMAT_ID: &'static str = "kd4.test-replacement-ledger.v2";
    pub const SEMANTIC_HASH_DOMAIN: &'static str = "kd4.test-replacement-ledger.v2.semantic";
    pub const SELF_HASH_DOMAIN: &'static str = "kd4.test-replacement-ledger.v2.self";

    pub fn validate(&self) -> Result<(), ContractError> {
        crate::canonical::canonical_jcs_of(self)?;
        if self.schema_version != 2 || self.format_id != Self::FORMAT_ID {
            return Err(ContractError::InvalidContract(
                "invalid TestReplacementLedgerV2 envelope".to_owned(),
            ));
        }
        for row in &self.rows {
            validate_nonempty_nfc(&row.obligation_id, "ledger obligation ID")?;
            if let ReplacementLedgerDispositionV2::Replacement {
                contract,
                edge_ids,
                stage2_incorrect_behavior_ids,
            } = &row.disposition
            {
                validate_sorted_unique_nfc_strings(edge_ids, "replacement edge IDs", true)?;
                validate_stage2_incorrect_behavior_ids(stage2_incorrect_behavior_ids)?;
                contract.validate()?;
                match (stage2_incorrect_behavior_ids.reviewed_ids(), contract) {
                    (Some(ids), ReplacementContractDispositionV1::Accepted { accepted, .. })
                        if ids.is_empty()
                            && accepted.resolved_entry_set_sha256.is_none()
                            && accepted.selection_v1_sha256.is_none()
                            && accepted.trusted_defect_receipt_sha256s.is_none() => {}
                    (Some(ids), ReplacementContractDispositionV1::Accepted { accepted, .. })
                        if !ids.is_empty()
                            && accepted.resolved_entry_set_sha256.is_some()
                            && accepted.selection_v1_sha256.is_some()
                            && accepted
                                .trusted_defect_receipt_sha256s
                                .as_ref()
                                .is_some_and(|hashes| {
                                    !hashes.is_empty()
                                        && !hashes.windows(2).any(|pair| pair[0] >= pair[1])
                                }) => {}
                    (Some(_), _) => {
                        return Err(ContractError::InvalidContract(
                            "reviewed Stage2 state requires an accepted replacement with matching receipt bindings"
                                .to_owned(),
                        ));
                    }
                    (None, ReplacementContractDispositionV1::Accepted { .. }) => {
                        return Err(ContractError::InvalidContract(
                            "accepted replacements require reviewed Stage2 state ([] or defect IDs)"
                                .to_owned(),
                        ));
                    }
                    (None, _) => {}
                }
            }
            row.disposition.validate()?;
        }
        ensure_sorted_unique_jcs(&self.rows, "replacement ledger rows")?;
        self.validate_trusted_defect_receipt_closure()?;
        let semantic_sha256 = proof_hash(
            Self::SEMANTIC_HASH_DOMAIN,
            &ReplacementLedgerSemanticProjectionV2 {
                format_id: &self.format_id,
                inventory_authority: &self.inventory_authority,
                rows: &self.rows,
                schema_version: self.schema_version,
                trusted_defect_receipts: &self.trusted_defect_receipts,
            },
        )?;
        if semantic_sha256 != self.semantic_sha256 {
            return Err(ContractError::InvalidContract(
                "replacement ledger semantic hash mismatch".to_owned(),
            ));
        }
        let self_hash = proof_hash(
            Self::SELF_HASH_DOMAIN,
            &ReplacementLedgerSelfProjectionV2 {
                format_id: &self.format_id,
                inventory_authority: &self.inventory_authority,
                rows: &self.rows,
                schema_version: self.schema_version,
                semantic_sha256: &self.semantic_sha256,
                trusted_defect_receipts: &self.trusted_defect_receipts,
            },
        )?;
        if self_hash != self.self_hash {
            return Err(ContractError::InvalidContract(
                "replacement ledger self hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_trusted_defect_receipt_closure(&self) -> Result<(), ContractError> {
        use std::collections::BTreeMap;
        use std::collections::BTreeSet;

        let referenced_ids = self
            .rows
            .iter()
            .flat_map(|row| match &row.disposition {
                ReplacementLedgerDispositionV2::Replacement {
                    stage2_incorrect_behavior_ids,
                    ..
                } => stage2_incorrect_behavior_ids
                    .reviewed_ids()
                    .unwrap_or_default(),
                _ => &[],
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        let Some(receipts) = &self.trusted_defect_receipts else {
            if referenced_ids.is_empty() {
                return Ok(());
            }
            return Err(ContractError::InvalidContract(
                "Stage2 row IDs require top-level trusted defect receipts".to_owned(),
            ));
        };
        if receipts.is_empty()
            || receipts
                .windows(2)
                .any(|pair| pair[0].defect_id >= pair[1].defect_id)
        {
            return Err(ContractError::InvalidContract(
                "trusted defect receipts must be nonempty and sorted uniquely by defect ID"
                    .to_owned(),
            ));
        }
        let mut receipt_hashes = BTreeSet::new();
        let mut by_id = BTreeMap::new();
        for receipt in receipts {
            receipt.validate()?;
            if !receipt_hashes.insert(receipt.receipt_sha256.clone()) {
                return Err(ContractError::InvalidContract(
                    "trusted defect receipt hashes must be unique".to_owned(),
                ));
            }
            by_id.insert(receipt.defect_id.clone(), receipt);
        }
        if referenced_ids != by_id.keys().cloned().collect() {
            return Err(ContractError::InvalidContract(
                "ledger rows and trusted defect receipts must form an exact closed set".to_owned(),
            ));
        }
        for (defect_id, receipt) in &by_id {
            let rows = self
                .rows
                .iter()
                .filter(|row| {
                    matches!(&row.disposition,
                        ReplacementLedgerDispositionV2::Replacement { stage2_incorrect_behavior_ids, .. }
                        if stage2_incorrect_behavior_ids.reviewed_ids().is_some_and(|ids| ids.binary_search(defect_id).is_ok()))
                })
                .collect::<Vec<_>>();
            let mut baseline_ids = BTreeSet::new();
            for row in &rows {
                baseline_ids.insert(row.baseline_id.clone().ok_or_else(|| {
                    ContractError::InvalidContract(
                        "trusted defect receipt rows require baseline IDs".to_owned(),
                    )
                })?);
            }
            let baseline_ids = baseline_ids.into_iter().collect::<Vec<_>>();
            let obligations = rows
                .iter()
                .map(|row| row.obligation_id.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let edges = rows
                .iter()
                .flat_map(|row| match &row.disposition {
                    ReplacementLedgerDispositionV2::Replacement { edge_ids, .. } => {
                        edge_ids.as_slice()
                    }
                    _ => &[],
                })
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            if baseline_ids != receipt.baseline_ids
                || obligations != receipt.baseline_obligation_ids
                || edges != receipt.replacement_edge_ids
            {
                return Err(ContractError::InvalidContract(
                    "trusted defect receipt baseline, obligation, or edge closure mismatch"
                        .to_owned(),
                ));
            }
            for row in rows {
                let ReplacementLedgerDispositionV2::Replacement {
                    contract: ReplacementContractDispositionV1::Accepted { accepted, .. },
                    ..
                } = &row.disposition
                else {
                    return Err(ContractError::InvalidContract(
                        "trusted defect receipts bind only accepted replacements".to_owned(),
                    ));
                };
                let ReplacementLedgerDispositionV2::Replacement {
                    stage2_incorrect_behavior_ids,
                    ..
                } = &row.disposition
                else {
                    unreachable!("filtered Stage2 row")
                };
                let stage2_ids = stage2_incorrect_behavior_ids
                    .reviewed_ids()
                    .expect("filtered Stage2 row is reviewed");
                let mut row_receipt_hashes = stage2_ids
                    .iter()
                    .map(|id| {
                        by_id
                            .get(id)
                            .map(|bound| bound.receipt_sha256.clone())
                            .ok_or_else(|| {
                                ContractError::InvalidContract(
                                    "row references an unknown trusted defect receipt".to_owned(),
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                row_receipt_hashes.sort();
                if accepted.trusted_defect_receipt_sha256s.as_ref() != Some(&row_receipt_hashes)
                    || accepted.selection_v1_sha256.as_ref() != Some(&receipt.selection_v1_sha256)
                    || accepted.resolved_entry_set_sha256.as_ref()
                        != Some(&receipt.resolved_entry_set_sha256)
                {
                    return Err(ContractError::InvalidContract(
                        "accepted replacement does not exactly bind its defect receipt selection"
                            .to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }
}

pub fn validate_inventory_ledger_predecessor_closure(
    inventory: &FrozenTestInventoryV2,
    ledger: &TestReplacementLedgerV2,
    recovery_raw: &[u8],
    transition_receipts: &[crate::recovery::RecoveryTransitionReceiptV1],
    applicability_issuer: &crate::applicability::ActiveHostApplicabilityIssuerV1,
) -> Result<(), ContractError> {
    validate_inventory_ledger_predecessor_closure_with_recaptures(
        inventory,
        ledger,
        recovery_raw,
        transition_receipts,
        applicability_issuer,
        None,
        None,
        None,
    )
}

pub fn validate_inventory_ledger_predecessor_closure_with_recapture(
    inventory: &FrozenTestInventoryV2,
    ledger: &TestReplacementLedgerV2,
    recovery_raw: &[u8],
    transition_receipts: &[crate::recovery::RecoveryTransitionReceiptV1],
    applicability_issuer: &crate::applicability::ActiveHostApplicabilityIssuerV1,
    doctest_recapture_raw: Option<&[u8]>,
) -> Result<(), ContractError> {
    validate_inventory_ledger_predecessor_closure_with_recaptures(
        inventory,
        ledger,
        recovery_raw,
        transition_receipts,
        applicability_issuer,
        doctest_recapture_raw,
        None,
        None,
    )
}

pub fn validate_inventory_ledger_predecessor_closure_with_recaptures(
    inventory: &FrozenTestInventoryV2,
    ledger: &TestReplacementLedgerV2,
    recovery_raw: &[u8],
    transition_receipts: &[crate::recovery::RecoveryTransitionReceiptV1],
    applicability_issuer: &crate::applicability::ActiveHostApplicabilityIssuerV1,
    doctest_recapture_raw: Option<&[u8]>,
    unittest_recapture_raw: Option<&[u8]>,
    predecessor_ledger_raw: Option<&[u8]>,
) -> Result<(), ContractError> {
    inventory.validate()?;
    ledger.validate()?;
    let recovery_value = parse_canonical_jcs(recovery_raw)?;
    let recovery: crate::recovery::InventoryRecoveryAuthorityV1 =
        serde_json::from_value(recovery_value)
            .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
    recovery.validate()?;
    let expected_authority = crate::selection::InventoryAuthorityRefV1 {
        path: StrictRepositoryPathV1::parse(
            ".codex/validation/frozen-test-inventory-v2.json".to_owned(),
        )?,
        raw_sha256: inventory.authority.raw_sha256.clone(),
        semantic_sha256: inventory.authority.semantic_sha256.clone(),
        self_hash: inventory.authority.self_hash.clone(),
    };
    if ledger.inventory_authority != expected_authority {
        return Err(ContractError::InvalidContract(
            "replacement ledger does not bind the exact Inventory V2 authority".to_owned(),
        ));
    }
    let recovery_raw_sha256 = Sha256HexV1::parse(format!("{:x}", Sha256::digest(recovery_raw)))?;
    let expected_recovery_authority = crate::selection::InventoryAuthorityRefV1 {
        path: StrictRepositoryPathV1::parse(
            ".codex/validation/frozen-test-inventory-v2-recoveries.json".to_owned(),
        )?,
        raw_sha256: recovery_raw_sha256,
        semantic_sha256: recovery.semantic_sha256.clone(),
        self_hash: recovery.self_hash.clone(),
    };
    if inventory.recovery_authority != expected_recovery_authority {
        return Err(ContractError::InvalidContract(
            "inventory does not bind the exact recovery authority bytes".to_owned(),
        ));
    }
    let mut observed_baseline_ids = ledger
        .rows
        .iter()
        .filter_map(|row| row.baseline_id.clone())
        .collect::<Vec<_>>();
    observed_baseline_ids.sort();
    inventory
        .predecessor_reconciliation
        .validate_observed_baseline_ids(
            &observed_baseline_ids,
            "replacement ledger baseline rows",
        )?;
    let mut declarations_by_obligation = BTreeMap::new();
    let mut declarations_by_baseline = BTreeMap::new();
    let mut expected_baseline_by_obligation = BTreeMap::new();
    for declaration in &inventory.declaration_universe {
        let obligation_id = declaration.obligation_id()?;
        if declarations_by_obligation
            .insert(obligation_id.clone(), declaration)
            .is_some()
        {
            return Err(ContractError::InvalidContract(
                "inventory declarations have duplicate obligation IDs".to_owned(),
            ));
        }
        expected_baseline_by_obligation
            .insert(obligation_id, declaration.baseline_id().map(str::to_owned));
        if let Some(baseline_id) = declaration.baseline_id()
            && declarations_by_baseline
                .insert(baseline_id.to_owned(), declaration)
                .is_some()
        {
            return Err(ContractError::InvalidContract(
                "inventory declarations have duplicate baseline IDs".to_owned(),
            ));
        }
    }
    let mut rows_by_obligation = BTreeMap::new();
    for row in &ledger.rows {
        if rows_by_obligation
            .insert(row.obligation_id.clone(), row)
            .is_some()
        {
            return Err(ContractError::InvalidContract(
                "ledger has duplicate obligation rows".to_owned(),
            ));
        }
    }
    if !declarations_by_obligation
        .keys()
        .all(|obligation_id| rows_by_obligation.contains_key(obligation_id))
    {
        return Err(ContractError::InvalidContract(
            "ledger rows do not cover every inventory declaration obligation".to_owned(),
        ));
    }
    for (obligation_id, expected_baseline) in &expected_baseline_by_obligation {
        let row = rows_by_obligation
            .get(obligation_id)
            .expect("declaration obligations were checked above");
        if &row.baseline_id != expected_baseline {
            return Err(ContractError::InvalidContract(
                "ledger baseline identity does not match its exact inventory declaration"
                    .to_owned(),
            ));
        }
    }
    let mut receipts_by_hash = BTreeMap::new();
    for receipt in transition_receipts {
        receipt.validate()?;
        if receipts_by_hash
            .insert(receipt.receipt_sha256.to_string(), receipt)
            .is_some()
        {
            return Err(ContractError::InvalidContract(
                "duplicate recovery transition receipt".to_owned(),
            ));
        }
    }
    let mut expected_receipt_hashes = recovery
        .records
        .iter()
        .filter_map(|record| {
            matches!(record.state, crate::recovery::RecoveryStateV1::Resolved)
                .then(|| {
                    record
                        .transition_receipt_sha256
                        .as_ref()
                        .map(ToString::to_string)
                })
                .flatten()
        })
        .collect::<Vec<_>>();
    expected_receipt_hashes.sort();
    if receipts_by_hash.keys().cloned().collect::<Vec<_>>() != expected_receipt_hashes {
        return Err(ContractError::InvalidContract(
            "transition receipts do not exactly cover resolved recovery records".to_owned(),
        ));
    }
    let frozen_authority_sha256 = recovery.frozen_source_authority.semantic_sha256()?;
    let doctest_recapture = doctest_recapture_raw
        .map(|raw| {
            let value = parse_canonical_jcs(raw)?;
            let packet: crate::recovery::DoctestRecapturePacketV1 =
                serde_json::from_value(value)
                    .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
            packet.validate()?;
            Ok(packet)
        })
        .transpose()?;
    let unittest_recapture = unittest_recapture_raw
        .map(|raw| {
            let value = parse_canonical_jcs(raw)?;
            let packet: crate::recovery::UnittestRecapturePacketV1 = serde_json::from_value(value)
                .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
            packet.validate()?;
            Ok(packet)
        })
        .transpose()?;
    let predecessor_ledger: Option<serde_json::Value> = predecessor_ledger_raw
        .map(|raw| {
            let raw_sha256 = Sha256HexV1::parse(format!("{:x}", Sha256::digest(raw)))?;
            if raw_sha256
                != inventory
                    .predecessor_reconciliation
                    .frozen_ledger_raw_sha256
            {
                return Err(ContractError::InvalidContract(
                    "predecessor ledger raw SHA-256 mismatch".to_owned(),
                ));
            }
            serde_json::from_slice(raw)
                .map_err(|error| ContractError::InvalidJson(error.to_string()))
        })
        .transpose()?;

    let mut authority_state_records = recovery.records.clone();
    for record in &mut authority_state_records {
        if !matches!(record.state, crate::recovery::RecoveryStateV1::Resolved) {
            continue;
        }
        match record.kind {
            crate::recovery::RecoveryKindV1::Doctest => {
                let crate::recovery::RecoveryCurrentAuditV1::Doctest { raw_count, .. } =
                    &mut record.current_audit
                else {
                    unreachable!("recovery validation checked kind alignment")
                };
                *raw_count = None;
                let crate::recovery::RecoveryLegacyEvidenceV1::Doctest {
                    historical_raw_count,
                    ..
                } = &mut record.legacy_evidence
                else {
                    unreachable!("recovery validation checked kind alignment")
                };
                *historical_raw_count = None;
                record.pending_requirement =
                    Some(crate::recovery::RecoveryPendingRequirementV1::Doctest {
                        baseline_commit: recovery.frozen_source_authority.baseline_commit.clone(),
                        reasons: vec![
                            "historical-raw-count-unknown".to_owned(),
                            "off-host-recapture-required".to_owned(),
                        ],
                        required_package_targets: vec![
                            "codex-core::lib::codex_core".to_owned(),
                            "codex-rollout::lib::codex_rollout".to_owned(),
                            "codex-state::lib::codex_state".to_owned(),
                            "codex-tui::lib::codex_tui".to_owned(),
                        ],
                    });
            }
            crate::recovery::RecoveryKindV1::Unittest => {
                let packet = unittest_recapture.as_ref().ok_or_else(|| {
                    ContractError::InvalidContract(
                        "resolved unittest recovery requires the exact typed recapture packet"
                            .to_owned(),
                    )
                })?;
                let crate::recovery::RecoveryLegacyEvidenceV1::Unittest {
                    historical_subtest_call_count,
                    ..
                } = &mut record.legacy_evidence
                else {
                    unreachable!("recovery validation checked kind alignment")
                };
                *historical_subtest_call_count = None;
                record.pending_requirement =
                    Some(crate::recovery::RecoveryPendingRequirementV1::Unittest {
                        baseline_commit: recovery.frozen_source_authority.baseline_commit.clone(),
                        expected_parent_output_sha256s: packet.parent_recapture_outputs()?,
                        reasons: vec![
                            "historical-subtest-count-unknown".to_owned(),
                            "parent-output-recapture-required".to_owned(),
                        ],
                        required_parent_ids: packet
                            .parent_records
                            .iter()
                            .map(|parent| parent.baseline_id.clone())
                            .collect(),
                        required_parent_count: UNITTEST_V1_EXECUTABLE_PARENT_COUNT as u64,
                    });
            }
        }
        record.resolution = None;
        record.state = crate::recovery::RecoveryStateV1::Pending;
        record.transition_receipt_sha256 = None;
    }
    #[derive(Serialize)]
    struct RecoveryAuthorityPredecessorProjectionV1<'a> {
        format_id: &'a str,
        frozen_source_authority: &'a crate::recovery::FrozenSourceAuthorityV1,
        records: &'a [crate::recovery::InventoryRecoveryRecordV1],
        schema_version: u8,
    }
    for (index, record) in recovery.records.iter().enumerate() {
        if !matches!(record.state, crate::recovery::RecoveryStateV1::Resolved) {
            continue;
        }
        let receipt_hash = record
            .transition_receipt_sha256
            .as_ref()
            .expect("recovery validation requires resolved receipt hash");
        let receipt = receipts_by_hash
            .get(receipt_hash.as_str())
            .expect("receipt key sets were checked above");
        let authority_before = proof_hash(
            "kd4.inventory-recovery-authority.semantic.v1",
            &RecoveryAuthorityPredecessorProjectionV1 {
                format_id: &recovery.format_id,
                frozen_source_authority: &recovery.frozen_source_authority,
                records: &authority_state_records,
                schema_version: recovery.schema_version,
            },
        )?;
        if receipt.authority_before_semantic_sha256 != authority_before {
            return Err(ContractError::InvalidContract(
                "recovery transition does not bind the actual predecessor authority".to_owned(),
            ));
        }
        authority_state_records[index] = record.clone();
    }
    let mut recovered_parents = BTreeMap::<String, (Vec<String>, Sha256HexV1)>::new();
    let mut recovered_unittest_children = BTreeSet::new();
    for record in &recovery.records {
        if !matches!(record.state, crate::recovery::RecoveryStateV1::Resolved) {
            continue;
        }
        let resolution = record.resolution.as_ref().ok_or_else(|| {
            ContractError::InvalidContract("resolved recovery has no resolution".to_owned())
        })?;
        let receipt_hash = record.transition_receipt_sha256.as_ref().ok_or_else(|| {
            ContractError::InvalidContract("resolved recovery has no receipt hash".to_owned())
        })?;
        let receipt = receipts_by_hash
            .get(&receipt_hash.to_string())
            .ok_or_else(|| {
                ContractError::InvalidContract("resolved recovery receipt is absent".to_owned())
            })?;
        let mut child_pairs = resolution
            .child_sources
            .iter()
            .map(|child| Ok((child, child.obligation_id()?)))
            .collect::<Result<Vec<_>, ContractError>>()?;
        let mut child_ids = child_pairs
            .iter()
            .map(|(_, id)| id.clone())
            .collect::<Vec<_>>();
        child_ids.sort();
        if matches!(record.kind, crate::recovery::RecoveryKindV1::Doctest) {
            let packet = doctest_recapture.as_ref().ok_or_else(|| {
                ContractError::InvalidContract(
                    "resolved doctest recovery requires the exact typed recapture packet"
                        .to_owned(),
                )
            })?;
            let mut expected_child_pairs = packet
                .runs
                .iter()
                .flat_map(|run| &run.raw_occurrences)
                .map(|occurrence| {
                    let child = crate::recovery::RecoveredChildSourceV1 {
                        canonical_parameter_projection: None,
                        child_kind: crate::recovery::RecoveredChildKindV1::Doctest,
                        declared_site_id: None,
                        executable_identity: ExecutableIdentityV1::Test {
                            route_id: TestRouteIdV1::RustDoctest,
                            test_id: TestIdV1::parse(format!(
                                "{}::recovered-raw-occurrence:{}",
                                occurrence.parent_baseline_id, occurrence.parent_ordinal
                            ))?,
                            validation_id: ValidationIdV1::parse(
                                "rust.doctest.workspace".to_owned(),
                            )?,
                        },
                        gap_id: "gap.doctest-raw-versus-unique".to_owned(),
                        occurrence_ordinal: Some(occurrence.parent_ordinal),
                        parent_baseline_id: occurrence.parent_baseline_id.clone(),
                    };
                    Ok((child.obligation_id()?, child))
                })
                .collect::<Result<Vec<_>, ContractError>>()?;
            expected_child_pairs.sort_by(|left, right| left.0.cmp(&right.0));
            let expected_children = expected_child_pairs
                .into_iter()
                .map(|(_, child)| child)
                .collect::<Vec<_>>();
            let expected_parents = packet
                .parent_counts
                .iter()
                .map(|parent| parent.parent_baseline_id.clone())
                .collect::<Vec<_>>();
            if resolution.recapture_receipt_sha256 != packet.receipt_sha256
                || resolution.parent_container_ids != expected_parents
                || resolution.child_sources != expected_children
            {
                return Err(ContractError::InvalidContract(
                    "doctest recovery does not exactly materialize its typed recapture packet"
                        .to_owned(),
                ));
            }
        } else {
            let packet = unittest_recapture.as_ref().ok_or_else(|| {
                ContractError::InvalidContract(
                    "resolved unittest recovery requires the exact typed recapture packet"
                        .to_owned(),
                )
            })?;
            let expected_children = packet.recovered_child_sources()?;
            let expected_parents = packet
                .parent_records
                .iter()
                .map(|parent| parent.baseline_id.clone())
                .collect::<Vec<_>>();
            let expected_outputs = packet.parent_recapture_outputs()?;
            let historical_subtest_call_count = match &record.legacy_evidence {
                crate::recovery::RecoveryLegacyEvidenceV1::Unittest {
                    historical_subtest_call_count,
                    ..
                } => *historical_subtest_call_count,
                _ => unreachable!("recovery validation checked kind alignment"),
            };
            if resolution.recapture_receipt_sha256 != packet.receipt_sha256
                || resolution.parent_container_ids != expected_parents
                || resolution.parent_recapture_outputs != expected_outputs
                || resolution.child_sources != expected_children
                || historical_subtest_call_count
                    != Some(packet.total_counts.subtest_occurrence_count)
            {
                return Err(ContractError::InvalidContract(
                    "unittest recovery does not exactly materialize its typed recapture packet"
                        .to_owned(),
                ));
            }
            for child_id in &child_ids {
                if declarations_by_obligation.contains_key(child_id) {
                    return Err(ContractError::InvalidContract(
                        "unittest recovered child collides with a declaration obligation"
                            .to_owned(),
                    ));
                }
                if !recovered_unittest_children.insert(child_id.clone()) {
                    return Err(ContractError::InvalidContract(
                        "unittest recovered child appears more than once".to_owned(),
                    ));
                }
            }
        }
        if receipt.child_obligation_ids != child_ids
            || receipt.parent_container_ids != resolution.parent_container_ids
            || receipt.recapture_receipt_sha256 != resolution.recapture_receipt_sha256
            || receipt.frozen_source_authority_sha256 != frozen_authority_sha256
        {
            return Err(ContractError::InvalidContract(
                "recovery transition receipt does not bind its exact resolved record".to_owned(),
            ));
        }
        if !matches!(record.kind, crate::recovery::RecoveryKindV1::Doctest) {
            continue;
        }
        for parent in &resolution.parent_container_ids {
            let mut parent_children = child_pairs
                .iter_mut()
                .filter(|(child, _)| &child.parent_baseline_id == parent)
                .map(|(_, id)| id.clone())
                .collect::<Vec<_>>();
            parent_children.sort();
            if recovered_parents
                .insert(
                    parent.clone(),
                    (parent_children, receipt.receipt_sha256.clone()),
                )
                .is_some()
            {
                return Err(ContractError::InvalidContract(format!(
                    "recovery parent {parent:?} appears in more than one recovery record"
                )));
            }
        }
    }
    let expected_obligations = declarations_by_obligation
        .keys()
        .cloned()
        .chain(recovered_unittest_children.iter().cloned())
        .collect::<BTreeSet<_>>();
    if rows_by_obligation.keys().cloned().collect::<BTreeSet<_>>() != expected_obligations {
        return Err(ContractError::InvalidContract(
            "ledger rows do not exactly cover declaration and recovered-child obligations"
                .to_owned(),
        ));
    }
    for child_id in &recovered_unittest_children {
        let row = rows_by_obligation
            .get(child_id)
            .expect("ledger obligation set was checked above");
        if row.baseline_id.is_some()
            || !matches!(&row.disposition, ReplacementLedgerDispositionV2::Unresolved)
        {
            return Err(ContractError::InvalidContract(
                "recovered child obligation must be a separate baseline-null unresolved row"
                    .to_owned(),
            ));
        }
    }
    let mut observed_recovered_parents = BTreeSet::new();
    for row in &ledger.rows {
        let ReplacementLedgerDispositionV2::RecoveredContainer {
            child_obligation_ids,
            transition_receipt_sha256,
        } = &row.disposition
        else {
            continue;
        };
        let baseline_id = row.baseline_id.as_ref().ok_or_else(|| {
            ContractError::InvalidContract(
                "recovered-container row requires a baseline ID".to_owned(),
            )
        })?;
        let expected = recovered_parents.get(baseline_id).ok_or_else(|| {
            ContractError::InvalidContract(
                "recovered-container row has no resolved recovery parent".to_owned(),
            )
        })?;
        if child_obligation_ids != &expected.0 || transition_receipt_sha256 != &expected.1 {
            return Err(ContractError::InvalidContract(
                "recovered-container row does not bind its recovery transition".to_owned(),
            ));
        }
        observed_recovered_parents.insert(baseline_id.clone());
    }
    if observed_recovered_parents != recovered_parents.keys().cloned().collect() {
        return Err(ContractError::InvalidContract(
            "recovered-container rows do not exactly cover resolved recovery parents".to_owned(),
        ));
    }
    if let Some(packet) = &unittest_recapture {
        let predecessor_ledger = predecessor_ledger.as_ref().ok_or_else(|| {
            ContractError::InvalidContract(
                "unittest recovery requires exact predecessor ledger bytes".to_owned(),
            )
        })?;
        let predecessor_rows = predecessor_ledger
            .get("rows")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                ContractError::InvalidContract(
                    "predecessor ledger has no canonical row array".to_owned(),
                )
            })?;
        let parent_manifests = packet
            .subtest_manifests()?
            .into_iter()
            .map(|manifest| (manifest.parent_baseline_id.clone(), manifest))
            .collect::<BTreeMap<_, _>>();
        let packet_parents = packet
            .parent_records
            .iter()
            .map(|parent| (parent.baseline_id.clone(), parent))
            .collect::<BTreeMap<_, _>>();
        let unittest_parent_ids = declarations_by_baseline
            .iter()
            .filter_map(|(baseline_id, declaration)| {
                matches!(
                    &declaration.entry().executable_identity,
                    ExecutableIdentityV1::Test {
                        route_id: TestRouteIdV1::PythonUnittest,
                        ..
                    }
                )
                .then_some(baseline_id.clone())
            })
            .collect::<BTreeSet<_>>();
        let hidden_parent_ids = unittest_parent_ids
            .iter()
            .filter(|baseline_id| baseline_id.starts_with("hidden-at-freeze-v1::python-unittest::"))
            .cloned()
            .collect::<BTreeSet<_>>();
        let executable_parent_ids = unittest_parent_ids
            .difference(&hidden_parent_ids)
            .cloned()
            .collect::<BTreeSet<_>>();
        let packet_parent_ids = packet_parents.keys().cloned().collect::<BTreeSet<_>>();
        let manifest_parent_ids = parent_manifests.keys().cloned().collect::<BTreeSet<_>>();
        let hidden_parent_ids_for_hash = hidden_parent_ids.iter().cloned().collect::<Vec<_>>();
        if unittest_parent_ids.len() != UNITTEST_V1_LEDGER_PARENT_COUNT
            || executable_parent_ids.len() != UNITTEST_V1_EXECUTABLE_PARENT_COUNT
            || hidden_parent_ids.len() != UNITTEST_V1_HIDDEN_PARENT_COUNT
            || proof_hash(
                "kd4.unittest-hidden-ledger-parent-ids.v1",
                &hidden_parent_ids_for_hash,
            )?
            .as_str()
                != UNITTEST_V1_HIDDEN_PARENT_IDS_SHA256
            || packet_parent_ids != executable_parent_ids
            || manifest_parent_ids != executable_parent_ids
        {
            return Err(ContractError::InvalidContract(
                "unittest predecessor partition must preserve 909 ledger parents, 893 executable parents, and 16 hidden replacement-only parents"
                    .to_owned(),
            ));
        }
        if packet
            .recovered_child_sources()?
            .iter()
            .any(|child| hidden_parent_ids.contains(&child.parent_baseline_id))
        {
            return Err(ContractError::InvalidContract(
                "hidden-at-freeze unittest parents cannot become recovered children".to_owned(),
            ));
        }
        let mut predecessor_rows_by_baseline = BTreeMap::new();
        for predecessor_row in predecessor_rows {
            let Some(baseline_id) = predecessor_row
                .get("baseline_id")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            if unittest_parent_ids.contains(baseline_id)
                && predecessor_rows_by_baseline
                    .insert(baseline_id.to_owned(), predecessor_row)
                    .is_some()
            {
                return Err(ContractError::InvalidContract(
                    "predecessor ledger repeats a unittest parent".to_owned(),
                ));
            }
        }
        if predecessor_rows_by_baseline
            .keys()
            .ne(unittest_parent_ids.iter())
        {
            return Err(ContractError::InvalidContract(
                "predecessor ledger does not exactly cover the 909 unittest parents".to_owned(),
            ));
        }
        let mut replacement_ids = Vec::new();
        let mut executable_replacement_ids = Vec::new();
        let mut unresolved_ids = Vec::new();
        for baseline_id in &unittest_parent_ids {
            let declaration = *declarations_by_baseline.get(baseline_id).ok_or_else(|| {
                ContractError::InvalidContract(
                    "unittest recapture parent has no frozen inventory declaration".to_owned(),
                )
            })?;
            let InventoryDeclarationV2::FrozenBaseline {
                entry,
                predecessor_entry_sha256,
                ..
            } = declaration
            else {
                unreachable!("baseline map only contains frozen declarations")
            };
            if !matches!(
                &entry.executable_identity,
                ExecutableIdentityV1::Test {
                    route_id: TestRouteIdV1::PythonUnittest,
                    test_id,
                    validation_id,
                } if test_id.as_str() == baseline_id.as_str()
                    && validation_id == &entry.validation_id
            ) {
                return Err(ContractError::InvalidContract(
                    "unittest parent declaration does not bind its exact frozen identity"
                        .to_owned(),
                ));
            }
            if executable_parent_ids.contains(baseline_id) {
                let parent_record = packet_parents
                    .get(baseline_id)
                    .expect("packet parent set was checked above");
                let manifest = parent_manifests
                    .get(baseline_id)
                    .expect("packet manifest set was checked above");
                if predecessor_entry_sha256 != &parent_record.predecessor_entry_sha256
                    || !matches!(
                        &entry.runner_selector,
                        RunnerSelectorV1::PythonUnittest {
                            parent_test_id,
                            subtest_manifest_sha256,
                            ..
                        } if parent_test_id == &parent_record.native_id
                            && subtest_manifest_sha256 == &manifest.manifest_sha256
                    )
                {
                    return Err(ContractError::InvalidContract(
                        "executable unittest parent declaration does not bind its exact packet identity and manifest"
                            .to_owned(),
                    ));
                }
            }
            let obligation_id = declaration.obligation_id()?;
            let current = &rows_by_obligation
                .get(&obligation_id)
                .expect("declaration obligations were checked above")
                .disposition;
            let predecessor_row = predecessor_rows_by_baseline
                .get(baseline_id)
                .expect("predecessor row key set was checked above");
            match predecessor_row
                .get("resolution")
                .and_then(serde_json::Value::as_str)
            {
                Some("unresolved") => {
                    unresolved_ids.push(baseline_id.clone());
                    if !matches!(current, ReplacementLedgerDispositionV2::Unresolved) {
                        return Err(ContractError::InvalidContract(
                            "unittest unresolved parent disposition changed".to_owned(),
                        ));
                    }
                }
                Some("replacement") => {
                    replacement_ids.push(baseline_id.clone());
                    if executable_parent_ids.contains(baseline_id) {
                        executable_replacement_ids.push(baseline_id.clone());
                    }
                    let mut expected_replacement_ids = predecessor_row
                        .get("replacement_ids")
                        .and_then(serde_json::Value::as_array)
                        .ok_or_else(|| {
                            ContractError::InvalidContract(
                                "predecessor replacement has no replacement IDs".to_owned(),
                            )
                        })?
                        .iter()
                        .map(|value| {
                            value.as_str().map(str::to_owned).ok_or_else(|| {
                                ContractError::InvalidContract(
                                    "predecessor replacement ID is not a string".to_owned(),
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    expected_replacement_ids.sort();
                    let predecessor_row_sha256 =
                        proof_hash("kd4.frozen-v1-replacement-ledger-row.v1", predecessor_row)?;
                    let ReplacementLedgerDispositionV2::Replacement { contract, .. } = current
                    else {
                        return Err(ContractError::InvalidContract(
                            "unittest replacement parent disposition changed".to_owned(),
                        ));
                    };
                    let legacy_hint = contract.legacy_replacement_hint();
                    if legacy_hint.predecessor_row_sha256 != predecessor_row_sha256
                        || legacy_hint.replacement_ids != expected_replacement_ids
                    {
                        return Err(ContractError::InvalidContract(
                            "unittest replacement parent mapping changed".to_owned(),
                        ));
                    }
                }
                _ => {
                    return Err(ContractError::InvalidContract(
                        "unittest predecessor parent has an unknown disposition".to_owned(),
                    ));
                }
            }
        }
        if replacement_ids.len() != UNITTEST_V1_REPLACEMENT_PARENT_COUNT
            || proof_hash("kd4.unittest-parent-replacement-ids.v1", &replacement_ids)?.as_str()
                != UNITTEST_V1_REPLACEMENT_PARENT_IDS_SHA256
            || executable_replacement_ids.len() != UNITTEST_V1_EXECUTABLE_REPLACEMENT_PARENT_COUNT
            || proof_hash(
                "kd4.unittest-parent-replacement-ids.v1",
                &executable_replacement_ids,
            )?
            .as_str()
                != UNITTEST_V1_EXECUTABLE_REPLACEMENT_PARENT_IDS_SHA256
            || unresolved_ids.len() != UNITTEST_V1_UNRESOLVED_PARENT_COUNT
            || proof_hash("kd4.unittest-parent-unresolved-ids.v1", &unresolved_ids)?.as_str()
                != UNITTEST_V1_UNRESOLVED_PARENT_IDS_SHA256
            || !hidden_parent_ids
                .iter()
                .all(|parent| replacement_ids.binary_search(parent).is_ok())
        {
            return Err(ContractError::InvalidContract(
                "unittest parent dispositions do not preserve the exact 536 replacement (520 executable plus 16 hidden) and 373 unresolved split"
                    .to_owned(),
            ));
        }
    }
    for row in &ledger.rows {
        let ReplacementLedgerDispositionV2::Exception {
            exception:
                ExceptionDispositionV1::Accepted {
                    active_host_authority,
                    provenance_receipt,
                    tag,
                    ..
                },
        } = &row.disposition
        else {
            continue;
        };
        let Some(declaration) = declarations_by_obligation.get(&row.obligation_id) else {
            continue;
        };
        if row.baseline_id.is_none()
            && !matches!(
                declaration,
                InventoryDeclarationV2::MissingBaseline {
                    source_provenance,
                    ..
                } | InventoryDeclarationV2::PostBaselineCurrent {
                    source_provenance,
                    ..
                } if matches!(tag, ExceptionTagV1::OffHost | ExceptionTagV1::PlatformPending)
                    && provenance_receipt == source_provenance
            )
        {
            return Err(ContractError::InvalidContract(
                "baseline-null accepted exception must bind the exact nonbaseline declaration provenance"
                    .to_owned(),
            ));
        }
        applicability_issuer.validate_complete_authority(active_host_authority, inventory)?;
        let active_host_projection = &active_host_authority.body.target_applicability_projection;
        if active_host_authority.body.inventory_authority != ledger.inventory_authority
            || active_host_projection.inventory_authority != ledger.inventory_authority
        {
            return Err(ContractError::InvalidContract(
                "accepted exception authority does not bind the active inventory authority"
                    .to_owned(),
            ));
        }
        let entry = declaration.entry();
        let projection_entry = active_host_projection
            .entries
            .iter()
            .find(|projection_entry| {
                projection_entry.identity == entry.executable_identity
                    && projection_entry.platform_applicability_sha256
                        == entry.platform_applicability_sha256
            })
            .ok_or_else(|| {
                ContractError::InvalidContract(
                    "accepted exception is not present in its authenticated active-host projection"
                        .to_owned(),
                )
            })?;
        let rust_route = matches!(
            entry.test_route_id,
            Some(TestRouteIdV1::RustNextest | TestRouteIdV1::RustDoctest)
        );
        projection_entry
            .applicability_result
            .validate_for(rust_route, &entry.platform_applicability)?;
        if matches!(
            tag,
            ExceptionTagV1::OffHost | ExceptionTagV1::PlatformPending
        ) && projection_entry.applicability_result.verdict
            != crate::applicability::ApplicabilityVerdictV1::NotApplicable
        {
            return Err(ContractError::InvalidContract(
                "off-host/platform-pending exception requires authenticated not-applicable verdict"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}

impl ReplacementLedgerDispositionV2 {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::RecoveredContainer {
                child_obligation_ids,
                ..
            } => {
                validate_sorted_unique_nfc_strings(
                    child_obligation_ids,
                    "recovered child obligations",
                    true,
                )?;
            }
            Self::Exception { exception } => exception.validate()?,
            _ => {}
        }
        Ok(())
    }
}

impl ExceptionDispositionV1 {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::PendingLegacy {
                provenance_receipt,
                tag,
            } => {
                provenance_receipt.validate()?;
                validate_exception_provenance_kind(*tag, provenance_receipt.kind)
            }
            Self::Accepted {
                active_host_authority,
                provenance_receipt,
                receipt_sha256,
                tag,
            } => {
                active_host_authority.validate()?;
                provenance_receipt.validate()?;
                validate_exception_provenance_kind(*tag, provenance_receipt.kind)?;
                let expected = proof_hash(
                    "kd4.accepted-exception-receipt.v1",
                    &AcceptedExceptionHashProjectionV1 {
                        active_host_authority,
                        provenance_receipt,
                        tag: *tag,
                    },
                )?;
                if expected != *receipt_sha256 {
                    return Err(ContractError::InvalidContract(
                        "accepted exception receipt hash mismatch".to_owned(),
                    ));
                }
                Ok(())
            }
        }
    }
}

fn validate_exception_provenance_kind(
    tag: ExceptionTagV1,
    kind: ProvenanceKindV1,
) -> Result<(), ContractError> {
    let matches = matches!(
        (tag, kind),
        (ExceptionTagV1::Generated, ProvenanceKindV1::Generated)
            | (ExceptionTagV1::Protected, ProvenanceKindV1::Protected)
            | (ExceptionTagV1::LiveService, ProvenanceKindV1::LiveService)
            | (ExceptionTagV1::OffHost, ProvenanceKindV1::OffHost)
            | (
                ExceptionTagV1::PlatformPending,
                ProvenanceKindV1::PlatformPending
            )
    );
    if matches {
        Ok(())
    } else {
        Err(ContractError::InvalidContract(
            "exception tag and typed provenance kind disagree".to_owned(),
        ))
    }
}

#[derive(Serialize)]
struct AcceptedExceptionHashProjectionV1<'a> {
    active_host_authority: &'a crate::applicability::ActiveHostApplicabilityAuthorityV1,
    provenance_receipt: &'a ProvenanceReceiptV1,
    tag: ExceptionTagV1,
}

#[derive(Serialize)]
struct ReplacementLedgerSemanticProjectionV2<'a> {
    format_id: &'a str,
    inventory_authority: &'a crate::selection::InventoryAuthorityRefV1,
    rows: &'a [ReplacementLedgerRowV2],
    schema_version: u8,
    trusted_defect_receipts: &'a Option<Vec<crate::receipts::TrustedDefectReceiptV1>>,
}

#[derive(Serialize)]
struct ReplacementLedgerSelfProjectionV2<'a> {
    format_id: &'a str,
    inventory_authority: &'a crate::selection::InventoryAuthorityRefV1,
    rows: &'a [ReplacementLedgerRowV2],
    schema_version: u8,
    semantic_sha256: &'a Sha256HexV1,
    trusted_defect_receipts: &'a Option<Vec<crate::receipts::TrustedDefectReceiptV1>>,
}
