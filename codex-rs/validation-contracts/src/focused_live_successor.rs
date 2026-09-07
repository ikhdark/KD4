use crate::canonical::ContractError;
use crate::canonical::Sha256HexV1;
use crate::canonical::canonical_jcs_of;
use crate::canonical::parse_canonical_jcs;
use crate::canonical::proof_hash;
use crate::canonical::validate_identifier;
use crate::canonical::validate_nfc;
use crate::canonical::validate_nonempty_nfc;
use crate::inventory_v2::CargoTargetContextSpecV1;
use crate::inventory_v2::CargoTargetKindV1;
use crate::inventory_v2::ExecutionInputContractV1;
use crate::path::StrictRepositoryPathV1;
use crate::runner::RunnerSelectorV1;
use crate::runner::TestRouteIdV1;
use crate::selection::ExecutableIdentityV1;
use crate::selection::ResolvedExecutableEntryV1;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use uuid::Uuid;
use uuid::Variant;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FocusedInventoryFrameworkV1 {
    ArgumentCommentLintNative,
    JavascriptJest,
    PythonPytest,
    PythonUnittest,
    RustDoctest,
    RustNextest,
    WindowsSandboxSmoke,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FocusedInventoryPlatformV1 {
    Darwin,
    Linux,
    Windows,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusedCurrentInventoryRowV1 {
    pub baseline_id: String,
    pub framework: FocusedInventoryFrameworkV1,
    pub native_id: String,
    pub source: StrictRepositoryPathV1,
    pub ignored: bool,
    pub platforms: Vec<FocusedInventoryPlatformV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementSuccessorV1 {
    pub test_id: String,
    pub framework: String,
    pub native_id: String,
    pub source_path: StrictRepositoryPathV1,
    pub test_route_id: String,
    pub validation_id: String,
    pub runner_selector_sha256: Sha256HexV1,
    pub executable_identity_sha256: Sha256HexV1,
    pub execution_input_contract_sha256: Sha256HexV1,
    pub platform_applicability_sha256: Sha256HexV1,
}

impl ReplacementSuccessorV1 {
    fn validate(&self) -> Result<(), ContractError> {
        validate_nonempty_nfc(&self.test_id, "replacement successor test ID")?;
        validate_nonempty_nfc(&self.framework, "replacement successor framework")?;
        validate_nonempty_nfc(&self.native_id, "replacement successor native ID")?;
        validate_identifier(&self.test_route_id)?;
        validate_identifier(&self.validation_id)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementSuccessorCatalogV1 {
    pub format_id: String,
    pub schema_version: u32,
    pub successors: Vec<ReplacementSuccessorV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusedSuccessorOwnerMapRowV1 {
    pub successor_id: String,
    pub baseline_ids: Vec<String>,
}

pub fn successor_owner_map_sha256_v1(
    owner_map: &[FocusedSuccessorOwnerMapRowV1],
) -> Result<Sha256HexV1, ContractError> {
    for row in owner_map {
        validate_nonempty_nfc(&row.successor_id, "successor owner-map successor ID")?;
        if row.baseline_ids.is_empty() {
            return Err(ContractError::InvalidContract(
                "successor owner-map baseline IDs must be nonempty".to_owned(),
            ));
        }
        for baseline_id in &row.baseline_ids {
            validate_nonempty_nfc(baseline_id, "successor owner-map baseline ID")?;
        }
        if row.baseline_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ContractError::InvalidContract(
                "successor owner-map baseline IDs must be sorted and unique".to_owned(),
            ));
        }
    }
    if owner_map
        .windows(2)
        .any(|pair| pair[0].successor_id >= pair[1].successor_id)
    {
        return Err(ContractError::InvalidContract(
            "successor owner map must be sorted by unique successor ID".to_owned(),
        ));
    }
    proof_hash(
        FocusedLiveSuccessorCatalogV1::SUCCESSOR_OWNER_MAP_HASH_DOMAIN,
        owner_map,
    )
}

impl ReplacementSuccessorCatalogV1 {
    pub const FORMAT_ID: &'static str = "kd4.replacement-successor-catalog.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        canonical_jcs_of(self)?;
        if self.format_id != Self::FORMAT_ID || self.schema_version != 1 {
            return Err(ContractError::InvalidContract(
                "invalid replacement successor catalog envelope".to_owned(),
            ));
        }
        for successor in &self.successors {
            successor.validate()?;
        }
        if self
            .successors
            .windows(2)
            .any(|pair| pair[0].test_id >= pair[1].test_id)
        {
            return Err(ContractError::InvalidContract(
                "replacement successor catalog must be sorted by unique test ID".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InProcessJestDiscoveryV1 {
    pub observation_id: String,
    pub execution_id: String,
    pub runner_pid: u32,
    pub started_at: String,
    pub ended_at: String,
    pub discovered_count: u64,
    pub discovered_test_ids_sha256: Sha256HexV1,
}

impl InProcessJestDiscoveryV1 {
    pub const OBSERVATION_ID: &'static str = "inventory.sdk.typescript.jest";

    fn validate(&self) -> Result<(), ContractError> {
        if self.observation_id != Self::OBSERVATION_ID
            || self.runner_pid == 0
            || self.discovered_count > MAX_SAFE_INTEGER
        {
            return Err(ContractError::InvalidContract(
                "invalid in-process Jest discovery envelope".to_owned(),
            ));
        }
        validate_canonical_uuid_v4(&self.execution_id, "in-process Jest execution ID")?;
        let started_at = parse_positive_u64_decimal(&self.started_at, "Jest start timestamp")?;
        let ended_at = parse_positive_u64_decimal(&self.ended_at, "Jest end timestamp")?;
        if started_at > ended_at {
            return Err(ContractError::InvalidContract(
                "in-process Jest discovery timestamps are reversed".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StableWindowsFileIdentityV1 {
    pub kind: String,
    pub volume_serial_number_hex: String,
    pub file_id_hex: String,
}

impl StableWindowsFileIdentityV1 {
    pub const KIND: &'static str = "windows-file-id-info-v1";

    fn validate(&self) -> Result<(), ContractError> {
        if self.kind != Self::KIND
            || !is_lower_hex_of_len(&self.volume_serial_number_hex, 16)
            || !is_lower_hex_of_len(&self.file_id_hex, 32)
            || self.file_id_hex == "0".repeat(32)
            || self.file_id_hex == "f".repeat(32)
        {
            return Err(ContractError::InvalidContract(
                "invalid stable Windows file identity".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryDiscoveryLaunchTargetIdentityV1 {
    pub requested: String,
    pub resolved_path: String,
    pub sha256_before: Sha256HexV1,
    pub sha256_after: Sha256HexV1,
}

impl InventoryDiscoveryLaunchTargetIdentityV1 {
    fn validate(&self) -> Result<(), ContractError> {
        validate_nonempty_nfc(&self.requested, "discovery executable request")?;
        validate_windows_absolute_file_path(&self.resolved_path, "resolved discovery executable")?;
        if self.sha256_before != self.sha256_after {
            return Err(ContractError::InvalidContract(
                "discovery executable identity changed during execution".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryDiscoveryChildProcessV1 {
    pub validation_id: String,
    pub execution_id: String,
    pub pid: u32,
    pub executable: String,
    pub launch_target_identity: InventoryDiscoveryLaunchTargetIdentityV1,
    pub args_hash: Sha256HexV1,
    pub started_at: String,
    pub ended_at: String,
    pub exit_code: i32,
}

impl InventoryDiscoveryChildProcessV1 {
    fn validate(&self, argv: &[String]) -> Result<(), ContractError> {
        validate_identifier(&self.validation_id)?;
        validate_canonical_uuid_v4(&self.execution_id, "inventory discovery execution ID")?;
        validate_windows_absolute_file_path(&self.executable, "inventory discovery executable")?;
        self.launch_target_identity.validate()?;
        let started_at = parse_positive_u64_decimal(
            &self.started_at,
            "inventory discovery process start timestamp",
        )?;
        let ended_at = parse_positive_u64_decimal(
            &self.ended_at,
            "inventory discovery process end timestamp",
        )?;
        if self.pid == 0
            || self.exit_code != 0
            || started_at > ended_at
            || self.executable != self.launch_target_identity.resolved_path
            || raw_sha256(argv)? != self.args_hash
        {
            return Err(ContractError::InvalidContract(
                "invalid inventory discovery child process".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum InventoryDiscoveryOutputV1 {
    Stdout {
        stdout_sha256: Sha256HexV1,
    },
    ReportFile {
        report_path: String,
        report_identity: StableWindowsFileIdentityV1,
        report_sha256: Sha256HexV1,
    },
}

impl InventoryDiscoveryOutputV1 {
    fn validate(&self, argv: &[String]) -> Result<(), ContractError> {
        match self {
            Self::Stdout { .. } => Ok(()),
            Self::ReportFile {
                report_path,
                report_identity,
                ..
            } => {
                validate_windows_absolute_file_path(
                    report_path,
                    "inventory discovery report path",
                )?;
                let output_positions = argv
                    .iter()
                    .enumerate()
                    .filter_map(|(index, argument)| (argument == "--output").then_some(index))
                    .collect::<Vec<_>>();
                if output_positions.len() != 1
                    || argv.get(output_positions[0] + 1) != Some(report_path)
                {
                    return Err(ContractError::InvalidContract(
                        "inventory discovery report path must exactly match the --output argument"
                            .to_owned(),
                    ));
                }
                report_identity.validate()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryDiscoveryProcessV1 {
    pub role: String,
    pub child_process: InventoryDiscoveryChildProcessV1,
    pub argv: Vec<String>,
    pub cwd: String,
    pub output: InventoryDiscoveryOutputV1,
}

impl InventoryDiscoveryProcessV1 {
    fn validate(&self, expected_role: &str, report_file: bool) -> Result<(), ContractError> {
        if self.role != expected_role || self.child_process.validation_id != expected_role {
            return Err(ContractError::InvalidContract(
                "inventory discovery process role or validation ID is out of order".to_owned(),
            ));
        }
        if self.argv.is_empty() {
            return Err(ContractError::InvalidContract(
                "inventory discovery argv must be nonempty".to_owned(),
            ));
        }
        for argument in &self.argv {
            validate_nfc(argument)?;
        }
        validate_windows_absolute_path(&self.cwd, "inventory discovery working directory")?;
        self.child_process.validate(&self.argv)?;
        if self.argv[0] != self.child_process.executable {
            return Err(ContractError::InvalidContract(
                "inventory discovery argv[0] must equal the launched executable".to_owned(),
            ));
        }
        self.output.validate(&self.argv)?;
        if report_file != matches!(self.output, InventoryDiscoveryOutputV1::ReportFile { .. }) {
            return Err(ContractError::InvalidContract(
                "inventory discovery process output kind does not match its role".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryDiscoveryInvocationAuthorityProcessV1 {
    pub role: String,
    pub executable: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub output_kind: String,
    pub report_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryDiscoveryInvocationAuthorityV1 {
    pub expected_processes: Vec<InventoryDiscoveryInvocationAuthorityProcessV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusedCatalogAttemptBoundsV1 {
    pub attempt_id: String,
    pub runner_pid: u32,
    pub started_at: String,
    pub reconciliation_started_at: String,
    pub ended_at: String,
}

impl FocusedCatalogAttemptBoundsV1 {
    fn validate(&self) -> Result<(u64, u64, u64), ContractError> {
        validate_canonical_uuid_v7(&self.attempt_id, "focused catalog attempt bound ID")?;
        let started_at = parse_positive_u64_decimal(&self.started_at, "attempt start timestamp")?;
        let reconciliation_started_at = parse_positive_u64_decimal(
            &self.reconciliation_started_at,
            "reconciliation start timestamp",
        )?;
        let ended_at = parse_positive_u64_decimal(&self.ended_at, "attempt end timestamp")?;
        if self.runner_pid == 0
            || started_at > reconciliation_started_at
            || reconciliation_started_at > ended_at
        {
            return Err(ContractError::InvalidContract(
                "invalid focused catalog attempt bounds".to_owned(),
            ));
        }
        Ok((started_at, reconciliation_started_at, ended_at))
    }
}

pub fn parse_inventory_discovery_process_set_canonical_v1(
    bytes: &[u8],
) -> Result<Vec<InventoryDiscoveryProcessV1>, ContractError> {
    let value = parse_canonical_jcs(bytes)?;
    let processes = serde_json::from_value::<Vec<InventoryDiscoveryProcessV1>>(value)
        .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
    validate_inventory_discovery_process_set_v1(&processes)?;
    Ok(processes)
}

pub fn validate_inventory_discovery_process_set_v1(
    processes: &[InventoryDiscoveryProcessV1],
) -> Result<(), ContractError> {
    canonical_jcs_of(processes)?;
    const ROLES: [(&str, bool); 6] = [
        ("inventory.rust-nextest", false),
        ("inventory.rust-doctest", false),
        ("inventory.root-unittest", true),
        ("inventory.sdk-python-pytest", true),
        ("inventory.tools.argument-comment-lint.native", false),
        ("inventory.windows.sandbox-smoke", false),
    ];
    if processes.len() != ROLES.len() {
        return Err(ContractError::InvalidContract(
            "inventory discovery process set must contain exactly six processes".to_owned(),
        ));
    }
    let mut execution_ids = BTreeSet::new();
    for (process, (expected_role, report_file)) in processes.iter().zip(ROLES) {
        process.validate(expected_role, report_file)?;
        if !execution_ids.insert(&process.child_process.execution_id) {
            return Err(ContractError::InvalidContract(
                "inventory discovery execution IDs must be unique".to_owned(),
            ));
        }
    }
    Ok(())
}

pub fn inventory_discovery_processes_sha256_v1(
    processes: &[InventoryDiscoveryProcessV1],
) -> Result<Sha256HexV1, ContractError> {
    validate_inventory_discovery_process_set_v1(processes)?;
    proof_hash(
        FocusedLiveSuccessorCatalogV1::PROCESS_SET_HASH_DOMAIN,
        processes,
    )
}

pub fn validate_inventory_discovery_process_authority_v1(
    processes: &[InventoryDiscoveryProcessV1],
    invocation_authority: &InventoryDiscoveryInvocationAuthorityV1,
    attempt_bounds: &FocusedCatalogAttemptBoundsV1,
) -> Result<(), ContractError> {
    validate_inventory_discovery_process_set_v1(processes)?;
    canonical_jcs_of(invocation_authority)?;
    canonical_jcs_of(attempt_bounds)?;
    let (attempt_started, reconciliation_started, _) = attempt_bounds.validate()?;
    if invocation_authority.expected_processes.len() != processes.len() {
        return Err(ContractError::InvalidContract(
            "inventory discovery invocation authority must contain exactly six processes"
                .to_owned(),
        ));
    }
    for (process, expected) in processes
        .iter()
        .zip(&invocation_authority.expected_processes)
    {
        let process_started = parse_positive_u64_decimal(
            &process.child_process.started_at,
            "inventory discovery process start timestamp",
        )?;
        let process_ended = parse_positive_u64_decimal(
            &process.child_process.ended_at,
            "inventory discovery process end timestamp",
        )?;
        let (observed_kind, observed_report_path) = match &process.output {
            InventoryDiscoveryOutputV1::Stdout { .. } => ("stdout", None),
            InventoryDiscoveryOutputV1::ReportFile { report_path, .. } => {
                ("report-file", Some(report_path.as_str()))
            }
        };
        validate_identifier(&expected.role)?;
        validate_windows_absolute_file_path(&expected.executable, "trusted discovery executable")?;
        validate_windows_absolute_path(&expected.cwd, "trusted discovery working directory")?;
        if expected.argv.is_empty() {
            return Err(ContractError::InvalidContract(
                "trusted inventory discovery argv must be nonempty".to_owned(),
            ));
        }
        for argument in &expected.argv {
            validate_nfc(argument)?;
        }
        if let Some(report_path) = &expected.report_path {
            validate_windows_absolute_file_path(report_path, "trusted discovery report path")?;
        }
        if process_started < attempt_started
            || process_ended > reconciliation_started
            || process.role != expected.role
            || process.child_process.executable != expected.executable
            || process.argv != expected.argv
            || process.cwd != expected.cwd
            || observed_kind != expected.output_kind
            || observed_report_path != expected.report_path.as_deref()
            || (observed_kind == "stdout") != expected.report_path.is_none()
        {
            return Err(ContractError::InvalidContract(
                "inventory discovery process does not exactly match trusted authority or attempt bounds"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusedLiveSuccessorCatalogV1 {
    pub format_id: String,
    pub schema_version: u32,
    pub attempt_id: String,
    pub focused_validation_id: String,
    pub frozen_inventory_hash: Sha256HexV1,
    pub start_fingerprint: Sha256HexV1,
    pub start_mutation_epoch: u64,
    pub replacement_baseline_row_count: u64,
    pub distinct_successor_count: u64,
    pub successor_ids_sha256: Sha256HexV1,
    pub successor_owner_map_sha256: Sha256HexV1,
    pub current_inventory_count: u64,
    pub current_inventory_hash: Sha256HexV1,
    pub current_inventory: Vec<FocusedCurrentInventoryRowV1>,
    pub resolved_successor_entries: Vec<ResolvedExecutableEntryV1>,
    pub resolved_successor_entries_sha256: Sha256HexV1,
    pub execution_input_contracts: Vec<ExecutionInputContractV1>,
    pub cargo_target_context_specs: Vec<CargoTargetContextSpecV1>,
    pub replacement_successor_catalog: ReplacementSuccessorCatalogV1,
    pub in_process_jest_discovery: InProcessJestDiscoveryV1,
    pub inventory_discovery_processes_sha256: Sha256HexV1,
    pub semantic_sha256: Sha256HexV1,
}

impl FocusedLiveSuccessorCatalogV1 {
    pub const FORMAT_ID: &'static str = "kd4.focused-live-successor-catalog.v1";
    pub const FOCUSED_VALIDATION_ID: &'static str = "inventory.current-evidence";
    pub const SEMANTIC_HASH_DOMAIN: &'static str = "kd4.focused-live-successor-catalog.v1.semantic";
    pub const PROCESS_SET_HASH_DOMAIN: &'static str = "kd4.inventory-discovery-process-set.v1";
    pub const SUCCESSOR_ID_SET_HASH_DOMAIN: &'static str = "kd4.focused-live-successor-id-set.v1";
    pub const SUCCESSOR_OWNER_MAP_HASH_DOMAIN: &'static str =
        "kd4.focused-live-successor-owner-map.v1";
    pub const JEST_DISCOVERED_ID_SET_HASH_DOMAIN: &'static str =
        "kd4.in-process-jest-discovered-id-set.v1";
    pub const RESOLVED_ENTRY_SET_HASH_DOMAIN: &'static str = "kd4.resolved-executable-entry-set.v1";

    pub fn parse_canonical(bytes: &[u8]) -> Result<Self, ContractError> {
        let value = parse_canonical_jcs(bytes)?;
        let catalog: Self = serde_json::from_value(value)
            .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        canonical_jcs_of(self)?;
        if self.format_id != Self::FORMAT_ID
            || self.schema_version != 1
            || self.focused_validation_id != Self::FOCUSED_VALIDATION_ID
            || self.start_mutation_epoch > MAX_SAFE_INTEGER
            || self.replacement_baseline_row_count > MAX_SAFE_INTEGER
            || self.distinct_successor_count > MAX_SAFE_INTEGER
            || self.current_inventory_count > MAX_SAFE_INTEGER
        {
            return Err(ContractError::InvalidContract(
                "invalid focused live successor catalog envelope".to_owned(),
            ));
        }
        validate_canonical_uuid_v7(&self.attempt_id, "focused catalog attempt ID")?;
        self.validate_current_inventory()?;
        self.validate_resolved_successors()?;
        self.validate_execution_inputs()?;
        self.validate_cargo_contexts()?;
        self.replacement_successor_catalog.validate()?;
        self.in_process_jest_discovery.validate()?;
        self.validate_resource_closure()?;

        let successor_ids = self
            .replacement_successor_catalog
            .successors
            .iter()
            .map(|successor| successor.test_id.as_str())
            .collect::<Vec<_>>();
        if self.distinct_successor_count != successor_ids.len() as u64
            || proof_hash(Self::SUCCESSOR_ID_SET_HASH_DOMAIN, &successor_ids)?
                != self.successor_ids_sha256
        {
            return Err(ContractError::InvalidContract(
                "focused successor ID count or hash mismatch".to_owned(),
            ));
        }

        if self.semantic_sha256()? != self.semantic_sha256 {
            return Err(ContractError::InvalidContract(
                "focused live successor catalog semantic hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn semantic_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        let projection = FocusedLiveSuccessorCatalogSemanticProjectionV1 {
            format_id: &self.format_id,
            schema_version: self.schema_version,
            attempt_id: &self.attempt_id,
            focused_validation_id: &self.focused_validation_id,
            frozen_inventory_hash: &self.frozen_inventory_hash,
            start_fingerprint: &self.start_fingerprint,
            start_mutation_epoch: self.start_mutation_epoch,
            replacement_baseline_row_count: self.replacement_baseline_row_count,
            distinct_successor_count: self.distinct_successor_count,
            successor_ids_sha256: &self.successor_ids_sha256,
            successor_owner_map_sha256: &self.successor_owner_map_sha256,
            current_inventory_count: self.current_inventory_count,
            current_inventory_hash: &self.current_inventory_hash,
            current_inventory: &self.current_inventory,
            resolved_successor_entries: &self.resolved_successor_entries,
            resolved_successor_entries_sha256: &self.resolved_successor_entries_sha256,
            execution_input_contracts: &self.execution_input_contracts,
            cargo_target_context_specs: &self.cargo_target_context_specs,
            replacement_successor_catalog: &self.replacement_successor_catalog,
            in_process_jest_discovery: &self.in_process_jest_discovery,
            inventory_discovery_processes_sha256: &self.inventory_discovery_processes_sha256,
        };
        proof_hash(Self::SEMANTIC_HASH_DOMAIN, &projection)
    }

    pub fn validate_with_inventory_discovery_processes(
        &self,
        processes: &[InventoryDiscoveryProcessV1],
    ) -> Result<(), ContractError> {
        self.validate()?;
        if inventory_discovery_processes_sha256_v1(processes)?
            != self.inventory_discovery_processes_sha256
        {
            return Err(ContractError::InvalidContract(
                "focused catalog inventory discovery process set hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn validate_semantics(
        &self,
        trusted_frozen_inventory_hash: &Sha256HexV1,
        trusted_start_fingerprint: &Sha256HexV1,
        trusted_start_mutation_epoch: u64,
        trusted_replacement_baseline_row_count: u64,
        trusted_successor_owner_map: &[FocusedSuccessorOwnerMapRowV1],
        trusted_current_inventory: &[FocusedCurrentInventoryRowV1],
        trusted_resolved_successor_entries: &[ResolvedExecutableEntryV1],
        trusted_execution_input_contracts: &[ExecutionInputContractV1],
        trusted_cargo_target_context_specs: &[CargoTargetContextSpecV1],
        trusted_replacement_successor_catalog: &ReplacementSuccessorCatalogV1,
        trusted_jest_observation: &InProcessJestDiscoveryV1,
        trusted_jest_discovered_ids: &[String],
        trusted_inventory_discovery_processes: &[InventoryDiscoveryProcessV1],
        trusted_invocation_authority: &InventoryDiscoveryInvocationAuthorityV1,
        trusted_attempt_bounds: &FocusedCatalogAttemptBoundsV1,
    ) -> Result<(), ContractError> {
        self.validate_with_inventory_discovery_processes(trusted_inventory_discovery_processes)?;
        validate_inventory_discovery_process_authority_v1(
            trusted_inventory_discovery_processes,
            trusted_invocation_authority,
            trusted_attempt_bounds,
        )?;
        if trusted_replacement_baseline_row_count > MAX_SAFE_INTEGER
            || trusted_start_mutation_epoch > MAX_SAFE_INTEGER
            || self.frozen_inventory_hash != *trusted_frozen_inventory_hash
            || self.start_fingerprint != *trusted_start_fingerprint
            || self.start_mutation_epoch != trusted_start_mutation_epoch
            || self.replacement_baseline_row_count != trusted_replacement_baseline_row_count
            || self.attempt_id != trusted_attempt_bounds.attempt_id
            || self.current_inventory != trusted_current_inventory
            || self.resolved_successor_entries != trusted_resolved_successor_entries
            || self.execution_input_contracts != trusted_execution_input_contracts
            || self.cargo_target_context_specs != trusted_cargo_target_context_specs
            || self.replacement_successor_catalog != *trusted_replacement_successor_catalog
        {
            return Err(ContractError::InvalidContract(
                "focused catalog does not match the trusted ledger count or attempt".to_owned(),
            ));
        }
        if successor_owner_map_sha256_v1(trusted_successor_owner_map)?
            != self.successor_owner_map_sha256
            || trusted_successor_owner_map
                .iter()
                .map(|row| row.successor_id.as_str())
                .ne(self
                    .replacement_successor_catalog
                    .successors
                    .iter()
                    .map(|successor| successor.test_id.as_str()))
        {
            return Err(ContractError::InvalidContract(
                "trusted successor owner map does not bind the exact successor catalog".to_owned(),
            ));
        }
        let owned_baseline_ids = trusted_successor_owner_map
            .iter()
            .flat_map(|row| row.baseline_ids.iter())
            .collect::<BTreeSet<_>>();
        if owned_baseline_ids.len() as u64 != trusted_replacement_baseline_row_count {
            return Err(ContractError::InvalidContract(
                "trusted replacement row count does not match the owner-map baseline union"
                    .to_owned(),
            ));
        }
        trusted_jest_observation.validate()?;
        if self.in_process_jest_discovery != *trusted_jest_observation {
            return Err(ContractError::InvalidContract(
                "catalog Jest discovery does not match the trusted observation".to_owned(),
            ));
        }
        for test_id in trusted_jest_discovered_ids {
            validate_nonempty_nfc(test_id, "trusted Jest discovered test ID")?;
        }
        if trusted_jest_discovered_ids
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
            || trusted_jest_discovered_ids
                .iter()
                .map(String::as_str)
                .ne(trusted_current_inventory
                    .iter()
                    .filter(|row| row.framework == FocusedInventoryFrameworkV1::JavascriptJest)
                    .map(|row| row.baseline_id.as_str()))
            || self.in_process_jest_discovery.discovered_count
                != trusted_jest_discovered_ids.len() as u64
            || proof_hash(
                Self::JEST_DISCOVERED_ID_SET_HASH_DOMAIN,
                trusted_jest_discovered_ids,
            )? != self.in_process_jest_discovery.discovered_test_ids_sha256
        {
            return Err(ContractError::InvalidContract(
                "trusted Jest discovered ID set does not match the catalog".to_owned(),
            ));
        }
        let (attempt_started, reconciliation_started, _) = trusted_attempt_bounds.validate()?;
        let jest_started = parse_positive_u64_decimal(
            &self.in_process_jest_discovery.started_at,
            "Jest start timestamp",
        )?;
        let jest_ended = parse_positive_u64_decimal(
            &self.in_process_jest_discovery.ended_at,
            "Jest end timestamp",
        )?;
        if self.in_process_jest_discovery.runner_pid != trusted_attempt_bounds.runner_pid
            || jest_started < attempt_started
            || jest_ended > reconciliation_started
            || trusted_inventory_discovery_processes.iter().any(|process| {
                process.child_process.execution_id == self.in_process_jest_discovery.execution_id
            })
        {
            return Err(ContractError::InvalidContract(
                "in-process Jest discovery does not match trusted attempt bounds".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_current_inventory(&self) -> Result<(), ContractError> {
        if self.current_inventory_count != self.current_inventory.len() as u64 {
            return Err(ContractError::InvalidContract(
                "focused catalog current inventory count mismatch".to_owned(),
            ));
        }
        for row in &self.current_inventory {
            validate_nonempty_nfc(&row.baseline_id, "current inventory baseline ID")?;
            validate_nonempty_nfc(&row.native_id, "current inventory native ID")?;
            if row.platforms.is_empty() || row.platforms.windows(2).any(|pair| pair[0] >= pair[1]) {
                return Err(ContractError::InvalidContract(
                    "current inventory platforms must be nonempty, sorted, and unique".to_owned(),
                ));
            }
        }
        if self
            .current_inventory
            .windows(2)
            .any(|pair| pair[0].baseline_id >= pair[1].baseline_id)
        {
            return Err(ContractError::InvalidContract(
                "current inventory must be sorted by unique baseline ID".to_owned(),
            ));
        }
        let projection = CurrentInventoryRawHashProjectionV1 {
            schema_version: 1,
            tests: &self.current_inventory,
        };
        if raw_sha256(&projection)? != self.current_inventory_hash {
            return Err(ContractError::InvalidContract(
                "focused catalog current inventory hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_resolved_successors(&self) -> Result<(), ContractError> {
        for entry in &self.resolved_successor_entries {
            entry.validate()?;
        }
        ensure_sorted_unique_jcs(
            &self.resolved_successor_entries,
            "resolved successor entries",
        )?;
        if proof_hash(
            Self::RESOLVED_ENTRY_SET_HASH_DOMAIN,
            &self.resolved_successor_entries,
        )? != self.resolved_successor_entries_sha256
        {
            return Err(ContractError::InvalidContract(
                "resolved successor entry set hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_execution_inputs(&self) -> Result<(), ContractError> {
        for contract in &self.execution_input_contracts {
            contract.validate()?;
        }
        ensure_sorted_unique_jcs(&self.execution_input_contracts, "execution input contracts")?;
        Ok(())
    }

    fn validate_cargo_contexts(&self) -> Result<(), ContractError> {
        for spec in &self.cargo_target_context_specs {
            spec.validate()?;
        }
        if self
            .cargo_target_context_specs
            .windows(2)
            .any(|pair| pair[0].context_sha256 >= pair[1].context_sha256)
        {
            return Err(ContractError::InvalidContract(
                "Cargo target contexts must be sorted by unique hash".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_resource_closure(&self) -> Result<(), ContractError> {
        let successors = self
            .replacement_successor_catalog
            .successors
            .iter()
            .map(|successor| (successor.test_id.as_str(), successor))
            .collect::<BTreeMap<_, _>>();
        let current_inventory = self
            .current_inventory
            .iter()
            .map(|row| (row.baseline_id.as_str(), row))
            .collect::<BTreeMap<_, _>>();
        let mut resolved_entries = BTreeMap::new();
        for resolved in &self.resolved_successor_entries {
            let entry = resolved.inventory_entry.as_ref();
            let test_id = match &entry.executable_identity {
                ExecutableIdentityV1::Test { test_id, .. } => test_id.as_str(),
                ExecutableIdentityV1::Action { .. } => {
                    return Err(ContractError::InvalidContract(
                        "resolved successor entries must carry test identities".to_owned(),
                    ));
                }
            };
            if resolved_entries.insert(test_id, entry).is_some() {
                return Err(ContractError::InvalidContract(
                    "resolved successor test identities must be unique".to_owned(),
                ));
            }
        }
        if resolved_entries.keys().ne(successors.keys()) {
            return Err(ContractError::InvalidContract(
                "resolved successor entries must exactly cover the successor catalog".to_owned(),
            ));
        }

        for (test_id, successor) in &successors {
            let entry = resolved_entries[test_id];
            let current = current_inventory
                .get(test_id)
                .or_else(|| {
                    // Historical unittest mappings may retain their native label.
                    // Bind it to the exact collected canonical inventory identity.
                    current_inventory
                        .get(format!("python-unittest::{test_id}").as_str())
                        .filter(|row| {
                            matches!(row.framework, FocusedInventoryFrameworkV1::PythonUnittest)
                                && row.native_id == *test_id
                        })
                })
                .ok_or_else(|| {
                    ContractError::InvalidContract(format!(
                        "successor {test_id} is absent from current inventory"
                    ))
                })?;
            match (&entry.executable_identity, entry.test_route_id) {
                (
                    ExecutableIdentityV1::Test {
                        route_id: identity_route_id,
                        validation_id: identity_validation_id,
                        ..
                    },
                    Some(entry_route_id),
                ) if *identity_route_id == entry_route_id
                    && identity_validation_id == &entry.validation_id => {}
                _ => {
                    return Err(ContractError::InvalidContract(format!(
                        "resolved successor identity route/validation disagrees with entry {test_id}"
                    )));
                }
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
            if selector_context != entry.cargo_target_context_spec_sha256.as_ref() {
                return Err(ContractError::InvalidContract(format!(
                    "resolved successor selector and entry Cargo context disagree for {test_id}"
                )));
            }
            if matches!(entry.runner_selector, RunnerSelectorV1::RustDoctest { .. }) {
                let context_sha256 = selector_context.expect("Rust doctest selector has a context");
                let context = self
                    .cargo_target_context_specs
                    .iter()
                    .find(|context| &context.context_sha256 == context_sha256)
                    .ok_or_else(|| {
                        ContractError::InvalidContract(format!(
                            "resolved Rust doctest successor {test_id} has no Cargo context"
                        ))
                    })?;
                if !matches!(
                    context.target_kind,
                    CargoTargetKindV1::Lib | CargoTargetKindV1::ProcMacro
                ) {
                    return Err(ContractError::InvalidContract(format!(
                        "resolved Rust doctest successor {test_id} requires a lib or proc-macro context"
                    )));
                }
            }
            let entry_route = entry
                .test_route_id
                .as_ref()
                .map(serialized_string)
                .transpose()?
                .ok_or_else(|| {
                    ContractError::InvalidContract(
                        "resolved successor entry is missing a test route".to_owned(),
                    )
                })?;
            let framework = serialized_string(&current.framework)?;
            let framework_route_selector_matches = matches!(
                (
                    &current.framework,
                    entry.test_route_id.as_ref(),
                    &entry.runner_selector,
                ),
                (
                    FocusedInventoryFrameworkV1::ArgumentCommentLintNative,
                    Some(TestRouteIdV1::ArgumentCommentLintNative),
                    RunnerSelectorV1::ArgumentCommentLintNative { .. },
                ) | (
                    FocusedInventoryFrameworkV1::JavascriptJest,
                    Some(TestRouteIdV1::JavascriptJest),
                    RunnerSelectorV1::JavascriptJest { .. },
                ) | (
                    FocusedInventoryFrameworkV1::PythonPytest,
                    Some(TestRouteIdV1::PythonPytest),
                    RunnerSelectorV1::PythonPytest { .. },
                ) | (
                    FocusedInventoryFrameworkV1::PythonUnittest,
                    Some(TestRouteIdV1::PythonUnittest),
                    RunnerSelectorV1::PythonUnittest { .. },
                ) | (
                    FocusedInventoryFrameworkV1::RustDoctest,
                    Some(TestRouteIdV1::RustDoctest),
                    RunnerSelectorV1::RustDoctest { .. },
                ) | (
                    FocusedInventoryFrameworkV1::RustNextest,
                    Some(TestRouteIdV1::RustNextest),
                    RunnerSelectorV1::RustNextest { .. },
                ) | (
                    FocusedInventoryFrameworkV1::WindowsSandboxSmoke,
                    Some(TestRouteIdV1::WindowsSandboxSmokeNative),
                    RunnerSelectorV1::WindowsSandboxSmokeNative { .. },
                )
            );
            if !framework_route_selector_matches {
                return Err(ContractError::InvalidContract(format!(
                    "current inventory framework disagrees with resolved route or selector for {test_id}"
                )));
            }
            if successor.test_route_id != entry_route
                || successor.validation_id != entry.validation_id.as_str()
                || successor.runner_selector_sha256 != entry.runner_selector_sha256
                || successor.executable_identity_sha256 != entry.executable_identity_sha256
                || successor.execution_input_contract_sha256
                    != entry.execution_input_contract_sha256
                || successor.platform_applicability_sha256 != entry.platform_applicability_sha256
                || successor.framework != framework
                || successor.native_id != current.native_id
                || successor.source_path != current.source
            {
                return Err(ContractError::InvalidContract(format!(
                    "successor catalog bindings disagree with resolved/current entry {test_id}"
                )));
            }
        }

        let contract_hashes = self
            .execution_input_contracts
            .iter()
            .map(|contract| &contract.contract_sha256)
            .collect::<BTreeSet<_>>();
        let referenced_contracts = resolved_entries
            .values()
            .map(|entry| &entry.execution_input_contract_sha256)
            .collect::<BTreeSet<_>>();
        if contract_hashes != referenced_contracts {
            return Err(ContractError::InvalidContract(
                "execution input contracts must be the exact resolved resource set".to_owned(),
            ));
        }

        let context_hashes = self
            .cargo_target_context_specs
            .iter()
            .map(|context| &context.context_sha256)
            .collect::<BTreeSet<_>>();
        let referenced_contexts = resolved_entries
            .values()
            .filter_map(|entry| entry.cargo_target_context_spec_sha256.as_ref())
            .collect::<BTreeSet<_>>();
        if context_hashes != referenced_contexts {
            return Err(ContractError::InvalidContract(
                "Cargo target contexts must be the exact resolved resource set".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct CurrentInventoryRawHashProjectionV1<'a> {
    schema_version: u32,
    tests: &'a [FocusedCurrentInventoryRowV1],
}

#[derive(Serialize)]
struct FocusedLiveSuccessorCatalogSemanticProjectionV1<'a> {
    format_id: &'a str,
    schema_version: u32,
    attempt_id: &'a str,
    focused_validation_id: &'a str,
    frozen_inventory_hash: &'a Sha256HexV1,
    start_fingerprint: &'a Sha256HexV1,
    start_mutation_epoch: u64,
    replacement_baseline_row_count: u64,
    distinct_successor_count: u64,
    successor_ids_sha256: &'a Sha256HexV1,
    successor_owner_map_sha256: &'a Sha256HexV1,
    current_inventory_count: u64,
    current_inventory_hash: &'a Sha256HexV1,
    current_inventory: &'a [FocusedCurrentInventoryRowV1],
    resolved_successor_entries: &'a [ResolvedExecutableEntryV1],
    resolved_successor_entries_sha256: &'a Sha256HexV1,
    execution_input_contracts: &'a [ExecutionInputContractV1],
    cargo_target_context_specs: &'a [CargoTargetContextSpecV1],
    replacement_successor_catalog: &'a ReplacementSuccessorCatalogV1,
    in_process_jest_discovery: &'a InProcessJestDiscoveryV1,
    inventory_discovery_processes_sha256: &'a Sha256HexV1,
}

fn validate_canonical_uuid_v4(value: &str, label: &str) -> Result<(), ContractError> {
    let parsed = Uuid::parse_str(value).map_err(|_| {
        ContractError::InvalidContract(format!("{label} must be a canonical UUIDv4"))
    })?;
    if parsed.to_string() != value
        || parsed.get_version_num() != 4
        || parsed.get_variant() != Variant::RFC4122
    {
        return Err(ContractError::InvalidContract(format!(
            "{label} must be a canonical UUIDv4"
        )));
    }
    Ok(())
}

fn validate_canonical_uuid_v7(value: &str, label: &str) -> Result<(), ContractError> {
    let parsed = Uuid::parse_str(value).map_err(|_| {
        ContractError::InvalidContract(format!("{label} must be a canonical UUIDv7"))
    })?;
    if parsed.to_string() != value
        || parsed.get_version_num() != 7
        || parsed.get_variant() != Variant::RFC4122
    {
        return Err(ContractError::InvalidContract(format!(
            "{label} must be a canonical UUIDv7"
        )));
    }
    Ok(())
}

fn parse_positive_u64_decimal(value: &str, label: &str) -> Result<u64, ContractError> {
    if value.is_empty()
        || value != "0" && value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ContractError::InvalidContract(format!(
            "{label} must be a canonical positive u64 decimal string"
        )));
    }
    let parsed = value.parse::<u64>().map_err(|_| {
        ContractError::InvalidContract(format!(
            "{label} must be a canonical positive u64 decimal string"
        ))
    })?;
    if parsed == 0 {
        return Err(ContractError::InvalidContract(format!(
            "{label} must be a canonical positive u64 decimal string"
        )));
    }
    Ok(parsed)
}

fn is_lower_hex_of_len(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_windows_absolute_path(value: &str, label: &str) -> Result<(), ContractError> {
    validate_windows_path(value, label, false)
}

fn validate_windows_absolute_file_path(value: &str, label: &str) -> Result<(), ContractError> {
    validate_windows_path(value, label, true)
}

fn validate_windows_path(
    value: &str,
    label: &str,
    require_component: bool,
) -> Result<(), ContractError> {
    validate_nonempty_nfc(value, label)?;
    if value.contains('/')
        || value.chars().any(|character| {
            character <= '\u{1f}'
                || character == '\u{7f}'
                || matches!(character, '<' | '>' | '"' | '|' | '?' | '*')
        })
    {
        return Err(ContractError::InvalidContract(format!(
            "{label} must be a canonical absolute Windows path"
        )));
    }

    let components = if value.len() >= 3
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value.as_bytes()[1] == b':'
        && value.as_bytes()[2] == b'\\'
    {
        if value[3..].contains(':') {
            return Err(ContractError::InvalidContract(format!(
                "{label} must not contain an alternate data stream"
            )));
        }
        if value.len() == 3 {
            Vec::new()
        } else {
            value[3..].split('\\').collect::<Vec<_>>()
        }
    } else {
        return Err(ContractError::InvalidContract(format!(
            "{label} must be a canonical absolute Windows path"
        )));
    };

    if require_component && components.is_empty()
        || components.iter().any(|component| {
            component.is_empty()
                || *component == "."
                || *component == ".."
                || component.ends_with('.')
                || component.ends_with(' ')
                || is_reserved_windows_component(component)
        })
    {
        return Err(ContractError::InvalidContract(format!(
            "{label} contains a noncanonical Windows path component"
        )));
    }
    Ok(())
}

fn is_reserved_windows_component(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) || upper.len() == 4
        && (upper.starts_with("COM") || upper.starts_with("LPT"))
        && matches!(upper.as_bytes()[3], b'1'..=b'9')
}

fn serialized_string<T: Serialize>(value: &T) -> Result<String, ContractError> {
    serde_json::to_value(value)
        .map_err(|error| ContractError::InvalidJson(error.to_string()))
        .and_then(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                ContractError::InvalidContract(
                    "contract identifier did not serialize as a string".to_owned(),
                )
            })
        })
}

fn raw_sha256<T: Serialize + ?Sized>(value: &T) -> Result<Sha256HexV1, ContractError> {
    let canonical = canonical_jcs_of(value)?;
    Sha256HexV1::parse(format!("{:x}", Sha256::digest(canonical)))
}

fn ensure_sorted_unique_jcs<T: Serialize>(values: &[T], label: &str) -> Result<(), ContractError> {
    let encodings = values
        .iter()
        .map(canonical_jcs_of)
        .collect::<Result<Vec<_>, _>>()?;
    if encodings.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ContractError::InvalidContract(format!(
            "{label} must be JCS-sorted and unique"
        )));
    }
    Ok(())
}
