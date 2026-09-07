use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_validation_contracts::canonical::ContractError;
use codex_validation_contracts::canonical::Sha256HexV1;
use codex_validation_contracts::canonical::canonical_jcs;
use codex_validation_contracts::canonical::parse_canonical_jcs;
use codex_validation_contracts::canonical::proof_hash;
use codex_validation_contracts::focused_live_successor::FocusedCatalogAttemptBoundsV1;
use codex_validation_contracts::focused_live_successor::FocusedCurrentInventoryRowV1;
use codex_validation_contracts::focused_live_successor::FocusedLiveSuccessorCatalogV1;
use codex_validation_contracts::focused_live_successor::FocusedSuccessorOwnerMapRowV1;
use codex_validation_contracts::focused_live_successor::InProcessJestDiscoveryV1;
use codex_validation_contracts::focused_live_successor::InventoryDiscoveryChildProcessV1;
use codex_validation_contracts::focused_live_successor::InventoryDiscoveryInvocationAuthorityProcessV1;
use codex_validation_contracts::focused_live_successor::InventoryDiscoveryInvocationAuthorityV1;
use codex_validation_contracts::focused_live_successor::InventoryDiscoveryLaunchTargetIdentityV1;
use codex_validation_contracts::focused_live_successor::InventoryDiscoveryOutputV1;
use codex_validation_contracts::focused_live_successor::InventoryDiscoveryProcessV1;
use codex_validation_contracts::focused_live_successor::ReplacementSuccessorCatalogV1;
use codex_validation_contracts::focused_live_successor::inventory_discovery_processes_sha256_v1;
use codex_validation_contracts::focused_live_successor::successor_owner_map_sha256_v1;
use codex_validation_contracts::focused_live_successor::validate_inventory_discovery_process_authority_v1;
use codex_validation_contracts::focused_live_successor::validate_inventory_discovery_process_set_v1;
use codex_validation_contracts::inventory_v2::CargoTargetContextSpecV1;
use codex_validation_contracts::inventory_v2::ExecutionInputContractV1;
use codex_validation_contracts::selection::ResolvedExecutableEntryV1;
use serde::Deserialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

const ATTEMPT_ID: &str = "01890f4e-8000-7000-8000-000000000001";
const PROCESS_ROLES: [&str; 6] = [
    "inventory.rust-nextest",
    "inventory.rust-doctest",
    "inventory.root-unittest",
    "inventory.sdk-python-pytest",
    "inventory.tools.argument-comment-lint.native",
    "inventory.windows.sandbox-smoke",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Vectors {
    format_id: String,
    schema_version: u32,
    catalog_format_id: String,
    hash_domains: HashDomains,
    digest_rules: DigestRules,
    valid_vectors: Vec<ValidVector>,
    invalid_vectors: Vec<InvalidVector>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HashDomains {
    semantic_sha256: String,
    successor_ids_sha256: String,
    successor_owner_map_sha256: String,
    resolved_successor_entries_sha256: String,
    in_process_jest_discovered_ids_sha256: String,
    inventory_discovery_processes_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DigestRules {
    semantic_sha256: String,
    successor_ids_sha256: String,
    successor_owner_map_sha256: String,
    current_inventory_hash: String,
    resolved_successor_entries_sha256: String,
    in_process_jest_discovered_ids_sha256: String,
    inventory_discovery_processes_sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidVector {
    case: String,
    catalog: FocusedLiveSuccessorCatalogV1,
    inventory_discovery_processes: Vec<InventoryDiscoveryProcessV1>,
    successor_owner_map: Vec<FocusedSuccessorOwnerMapRowV1>,
    jest_observation: InProcessJestDiscoveryV1,
    jest_discovered_ids: Vec<String>,
    attempt_bounds: FocusedCatalogAttemptBoundsV1,
    invocation_authority: InventoryDiscoveryInvocationAuthorityV1,
    current_inventory: Vec<FocusedCurrentInventoryRowV1>,
    resolved_successor_entries: Vec<ResolvedExecutableEntryV1>,
    execution_input_contracts: Vec<ExecutionInputContractV1>,
    cargo_target_context_specs: Vec<CargoTargetContextSpecV1>,
    replacement_successor_catalog: ReplacementSuccessorCatalogV1,
    expected_replacement_baseline_row_count: u64,
    expected_frozen_inventory_hash: Sha256HexV1,
    expected_start_fingerprint: Sha256HexV1,
    expected_start_mutation_epoch: u64,
    canonical_catalog_json: String,
    canonical_inventory_discovery_processes_json: String,
    expected_hashes: ExpectedHashes,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedHashes {
    semantic_sha256: Sha256HexV1,
    successor_ids_sha256: Sha256HexV1,
    successor_owner_map_sha256: Sha256HexV1,
    current_inventory_hash: Sha256HexV1,
    resolved_successor_entries_sha256: Sha256HexV1,
    in_process_jest_discovered_ids_sha256: Sha256HexV1,
    inventory_discovery_processes_sha256: Sha256HexV1,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvalidVector {
    case: String,
    kind: String,
    api: String,
    #[serde(default)]
    raw_json_base64url: Option<String>,
    #[serde(default)]
    value: Option<Value>,
    expected: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessAuthorityVectorValue {
    processes: Vec<InventoryDiscoveryProcessV1>,
    invocation_authority: InventoryDiscoveryInvocationAuthorityV1,
    attempt_bounds: FocusedCatalogAttemptBoundsV1,
}

fn shared_vectors() -> Vectors {
    serde_json::from_slice(include_bytes!(
        "../../../scripts/fixtures/focused_live_successor_catalog_v1_vectors.json"
    ))
    .expect("focused catalog vectors parse")
}

fn digest(byte: u8) -> Sha256HexV1 {
    Sha256HexV1::parse(format!("{byte:02x}").repeat(32)).expect("test digest is valid")
}

fn raw_sha256<T: serde::Serialize + ?Sized>(value: &T) -> Sha256HexV1 {
    let value = serde_json::to_value(value).expect("test value serializes");
    let canonical = canonical_jcs(&value).expect("test value canonicalizes");
    Sha256HexV1::parse(format!("{:x}", Sha256::digest(canonical))).expect("SHA-256 output is valid")
}

fn process_evidence() -> (
    Vec<InventoryDiscoveryProcessV1>,
    InventoryDiscoveryInvocationAuthorityV1,
    FocusedCatalogAttemptBoundsV1,
) {
    let mut processes = Vec::new();
    let mut expected_processes = Vec::new();
    for (index, role) in PROCESS_ROLES.iter().enumerate() {
        let sequence = index + 1;
        let executable = format!(r"C:\tools\runner{sequence}.exe");
        let report_path =
            matches!(index, 2 | 3).then(|| format!(r"C:\attempt\report{sequence}.json"));
        let mut argv = vec![executable.clone(), (*role).to_owned()];
        if let Some(report_path) = &report_path {
            argv.extend(["--output".to_owned(), report_path.clone()]);
        }
        let output = match &report_path {
            Some(report_path) => InventoryDiscoveryOutputV1::ReportFile {
                report_path: report_path.clone(),
                report_identity:
                    codex_validation_contracts::focused_live_successor::StableWindowsFileIdentityV1 {
                        kind: "windows-file-id-info-v1".to_owned(),
                        volume_serial_number_hex: "0000000000000000".to_owned(),
                        file_id_hex: format!("{sequence:032x}"),
                    },
                report_sha256: digest(0x40 + index as u8),
            },
            None => InventoryDiscoveryOutputV1::Stdout {
                stdout_sha256: digest(0x50 + index as u8),
            },
        };
        let process = InventoryDiscoveryProcessV1 {
            role: (*role).to_owned(),
            child_process: InventoryDiscoveryChildProcessV1 {
                validation_id: (*role).to_owned(),
                execution_id: format!("00000000-0000-4000-8000-{sequence:012}"),
                pid: 1000 + sequence as u32,
                executable: executable.clone(),
                launch_target_identity: InventoryDiscoveryLaunchTargetIdentityV1 {
                    requested: format!("runner{sequence}"),
                    resolved_path: executable.clone(),
                    sha256_before: digest(0x20 + index as u8),
                    sha256_after: digest(0x20 + index as u8),
                },
                args_hash: raw_sha256(&argv),
                started_at: (110 + sequence).to_string(),
                ended_at: (120 + sequence).to_string(),
                exit_code: 0,
            },
            argv: argv.clone(),
            cwd: r"C:\".to_owned(),
            output,
        };
        expected_processes.push(InventoryDiscoveryInvocationAuthorityProcessV1 {
            role: (*role).to_owned(),
            executable,
            argv,
            cwd: r"C:\".to_owned(),
            output_kind: if report_path.is_some() {
                "report-file".to_owned()
            } else {
                "stdout".to_owned()
            },
            report_path,
        });
        processes.push(process);
    }
    (
        processes,
        InventoryDiscoveryInvocationAuthorityV1 { expected_processes },
        FocusedCatalogAttemptBoundsV1 {
            attempt_id: ATTEMPT_ID.to_owned(),
            runner_pid: 900,
            started_at: "100".to_owned(),
            reconciliation_started_at: "200".to_owned(),
            ended_at: "300".to_owned(),
        },
    )
}

fn empty_catalog(processes: &[InventoryDiscoveryProcessV1]) -> FocusedLiveSuccessorCatalogV1 {
    let empty_ids = Vec::<String>::new();
    let empty_resolved = Vec::<ResolvedExecutableEntryV1>::new();
    let current_inventory = Vec::new();
    let current_hash_projection = serde_json::json!({"schema_version": 1, "tests": []});
    let mut catalog = FocusedLiveSuccessorCatalogV1 {
        format_id: FocusedLiveSuccessorCatalogV1::FORMAT_ID.to_owned(),
        schema_version: 1,
        attempt_id: ATTEMPT_ID.to_owned(),
        focused_validation_id: FocusedLiveSuccessorCatalogV1::FOCUSED_VALIDATION_ID.to_owned(),
        frozen_inventory_hash: digest(1),
        start_fingerprint: digest(2),
        start_mutation_epoch: 0,
        replacement_baseline_row_count: 0,
        distinct_successor_count: 0,
        successor_ids_sha256: proof_hash(
            FocusedLiveSuccessorCatalogV1::SUCCESSOR_ID_SET_HASH_DOMAIN,
            &empty_ids,
        )
        .expect("empty successor IDs hash"),
        successor_owner_map_sha256: successor_owner_map_sha256_v1(&[])
            .expect("empty owner map hash"),
        current_inventory_count: 0,
        current_inventory_hash: raw_sha256(&current_hash_projection),
        current_inventory,
        resolved_successor_entries_sha256: proof_hash(
            FocusedLiveSuccessorCatalogV1::RESOLVED_ENTRY_SET_HASH_DOMAIN,
            &empty_resolved,
        )
        .expect("empty resolved set hash"),
        resolved_successor_entries: empty_resolved,
        execution_input_contracts: Vec::<ExecutionInputContractV1>::new(),
        cargo_target_context_specs: Vec::<CargoTargetContextSpecV1>::new(),
        replacement_successor_catalog: ReplacementSuccessorCatalogV1 {
            format_id: ReplacementSuccessorCatalogV1::FORMAT_ID.to_owned(),
            schema_version: 1,
            successors: Vec::new(),
        },
        in_process_jest_discovery: InProcessJestDiscoveryV1 {
            observation_id: InProcessJestDiscoveryV1::OBSERVATION_ID.to_owned(),
            execution_id: "00000000-0000-4000-8000-000000000007".to_owned(),
            runner_pid: 900,
            started_at: "105".to_owned(),
            ended_at: "125".to_owned(),
            discovered_count: 0,
            discovered_test_ids_sha256: proof_hash(
                FocusedLiveSuccessorCatalogV1::JEST_DISCOVERED_ID_SET_HASH_DOMAIN,
                &empty_ids,
            )
            .expect("empty Jest ID hash"),
        },
        inventory_discovery_processes_sha256: inventory_discovery_processes_sha256_v1(processes)
            .expect("process set hash"),
        semantic_sha256: digest(0),
    };
    catalog.semantic_sha256 = catalog.semantic_sha256().expect("catalog semantic hash");
    catalog
}

fn validate_vector_semantics(vector: &ValidVector) -> Result<(), String> {
    vector
        .catalog
        .validate_semantics(
            &vector.expected_frozen_inventory_hash,
            &vector.expected_start_fingerprint,
            vector.expected_start_mutation_epoch,
            vector.expected_replacement_baseline_row_count,
            &vector.successor_owner_map,
            &vector.current_inventory,
            &vector.resolved_successor_entries,
            &vector.execution_input_contracts,
            &vector.cargo_target_context_specs,
            &vector.replacement_successor_catalog,
            &vector.jest_observation,
            &vector.jest_discovered_ids,
            &vector.inventory_discovery_processes,
            &vector.invocation_authority,
            &vector.attempt_bounds,
        )
        .map_err(|error| error.to_string())
}

#[test]
fn shared_catalog_vectors_match_python() {
    let vectors = shared_vectors();
    assert_eq!(
        vectors.format_id,
        "kd4.focused-live-successor-catalog.v1.test-vectors"
    );
    assert_eq!(vectors.schema_version, 1);
    assert_eq!(
        vectors.catalog_format_id,
        FocusedLiveSuccessorCatalogV1::FORMAT_ID
    );
    let domains = &vectors.hash_domains;
    assert_eq!(
        domains.semantic_sha256,
        FocusedLiveSuccessorCatalogV1::SEMANTIC_HASH_DOMAIN
    );
    assert_eq!(
        domains.successor_ids_sha256,
        FocusedLiveSuccessorCatalogV1::SUCCESSOR_ID_SET_HASH_DOMAIN
    );
    assert_eq!(
        domains.successor_owner_map_sha256,
        FocusedLiveSuccessorCatalogV1::SUCCESSOR_OWNER_MAP_HASH_DOMAIN
    );
    assert_eq!(
        domains.resolved_successor_entries_sha256,
        FocusedLiveSuccessorCatalogV1::RESOLVED_ENTRY_SET_HASH_DOMAIN
    );
    assert_eq!(
        domains.in_process_jest_discovered_ids_sha256,
        FocusedLiveSuccessorCatalogV1::JEST_DISCOVERED_ID_SET_HASH_DOMAIN
    );
    assert_eq!(
        domains.inventory_discovery_processes_sha256,
        FocusedLiveSuccessorCatalogV1::PROCESS_SET_HASH_DOMAIN
    );
    for rule in [
        &vectors.digest_rules.semantic_sha256,
        &vectors.digest_rules.successor_ids_sha256,
        &vectors.digest_rules.successor_owner_map_sha256,
        &vectors.digest_rules.current_inventory_hash,
        &vectors.digest_rules.resolved_successor_entries_sha256,
        &vectors.digest_rules.in_process_jest_discovered_ids_sha256,
        &vectors.digest_rules.inventory_discovery_processes_sha256,
    ] {
        assert!(rule.starts_with("SHA256("));
        assert!(rule.contains("strict JCS"));
    }
    assert_eq!(vectors.valid_vectors.len(), 5);
    assert_eq!(vectors.invalid_vectors.len(), 68);

    for vector in &vectors.valid_vectors {
        vector
            .catalog
            .validate()
            .unwrap_or_else(|error| panic!("{} catalog must validate: {error}", vector.case));
        validate_vector_semantics(vector)
            .unwrap_or_else(|error| panic!("{} semantics must validate: {error}", vector.case));
        let catalog_value = serde_json::to_value(&vector.catalog).expect("catalog serializes");
        assert_eq!(
            canonical_jcs(&catalog_value).expect("catalog canonicalizes"),
            vector.canonical_catalog_json.as_bytes(),
            "{}",
            vector.case
        );
        let process_value = serde_json::to_value(&vector.inventory_discovery_processes)
            .expect("process evidence serializes");
        assert_eq!(
            canonical_jcs(&process_value).expect("process evidence canonicalizes"),
            vector
                .canonical_inventory_discovery_processes_json
                .as_bytes(),
            "{}",
            vector.case
        );
        assert_eq!(
            vector.catalog.semantic_sha256, vector.expected_hashes.semantic_sha256,
            "{}",
            vector.case
        );
        assert_eq!(
            vector.catalog.successor_ids_sha256, vector.expected_hashes.successor_ids_sha256,
            "{}",
            vector.case
        );
        assert_eq!(
            vector.catalog.successor_owner_map_sha256,
            vector.expected_hashes.successor_owner_map_sha256,
            "{}",
            vector.case
        );
        assert_eq!(
            vector.catalog.current_inventory_hash, vector.expected_hashes.current_inventory_hash,
            "{}",
            vector.case
        );
        assert_eq!(
            vector.catalog.resolved_successor_entries_sha256,
            vector.expected_hashes.resolved_successor_entries_sha256,
            "{}",
            vector.case
        );
        assert_eq!(
            vector
                .catalog
                .in_process_jest_discovery
                .discovered_test_ids_sha256,
            vector.expected_hashes.in_process_jest_discovered_ids_sha256,
            "{}",
            vector.case
        );
        assert_eq!(
            vector.catalog.inventory_discovery_processes_sha256,
            vector.expected_hashes.inventory_discovery_processes_sha256,
            "{}",
            vector.case
        );
        assert_eq!(
            successor_owner_map_sha256_v1(&vector.successor_owner_map)
                .expect("owner map hash computes"),
            vector.expected_hashes.successor_owner_map_sha256,
            "{}",
            vector.case
        );
        assert_eq!(
            inventory_discovery_processes_sha256_v1(&vector.inventory_discovery_processes)
                .expect("process hash computes"),
            vector.expected_hashes.inventory_discovery_processes_sha256,
            "{}",
            vector.case
        );
        assert_eq!(
            proof_hash(
                FocusedLiveSuccessorCatalogV1::JEST_DISCOVERED_ID_SET_HASH_DOMAIN,
                &vector.jest_discovered_ids,
            )
            .expect("Jest ID hash computes"),
            vector.expected_hashes.in_process_jest_discovered_ids_sha256,
            "{}",
            vector.case
        );
    }

    for vector in &vectors.invalid_vectors {
        assert_eq!(vector.expected, "reject", "{}", vector.case);
        assert!(!vector.reason.is_empty(), "{}", vector.case);
        let rejected = match (vector.kind.as_str(), vector.api.as_str()) {
            ("raw-json-bytes", "parse_focused_live_successor_catalog_v1") => {
                let raw = URL_SAFE_NO_PAD
                    .decode(
                        vector
                            .raw_json_base64url
                            .as_deref()
                            .expect("raw invalid vector carries bytes"),
                    )
                    .expect("invalid vector bytes are base64url");
                match parse_canonical_jcs(&raw) {
                    Err(_) => true,
                    Ok(value) => {
                        match serde_json::from_value::<FocusedLiveSuccessorCatalogV1>(value) {
                            Err(_) => true,
                            Ok(catalog) => catalog.validate().is_err(),
                        }
                    }
                }
            }
            ("value", "validate_focused_live_successor_catalog_wire_v1")
            | ("catalog", "validate_focused_live_successor_catalog_wire_v1") => {
                match serde_json::from_value::<FocusedLiveSuccessorCatalogV1>(
                    vector.value.clone().expect("wire vector carries a value"),
                ) {
                    Err(_) => true,
                    Ok(catalog) => catalog.validate().is_err(),
                }
            }
            ("process-set", "validate_inventory_discovery_process_set_v1") => {
                match serde_json::from_value::<Vec<InventoryDiscoveryProcessV1>>(
                    vector
                        .value
                        .clone()
                        .expect("process vector carries a value"),
                ) {
                    Err(_) => true,
                    Ok(processes) => {
                        validate_inventory_discovery_process_set_v1(&processes).is_err()
                    }
                }
            }
            ("process-authority", "validate_inventory_discovery_process_authority_v1") => {
                match serde_json::from_value::<ProcessAuthorityVectorValue>(
                    vector
                        .value
                        .clone()
                        .expect("process authority vector carries a value"),
                ) {
                    Err(_) => true,
                    Ok(value) => validate_inventory_discovery_process_authority_v1(
                        &value.processes,
                        &value.invocation_authority,
                        &value.attempt_bounds,
                    )
                    .is_err(),
                }
            }
            ("semantic-catalog", "validate_focused_live_successor_catalog_semantics_v1") => {
                match serde_json::from_value::<ValidVector>(
                    vector
                        .value
                        .clone()
                        .expect("semantic vector carries a value"),
                ) {
                    Err(_) => true,
                    Ok(value) => validate_vector_semantics(&value).is_err(),
                }
            }
            route => panic!("unknown invalid vector route {route:?}"),
        };
        assert!(rejected, "{}", vector.case);
    }
}

#[test]
fn focused_catalog_atomic_canonical_parser_closes_the_typed_contract() {
    let vectors = shared_vectors();
    let catalog = &vectors.valid_vectors[0].catalog;
    let canonical = canonical_jcs(
        &serde_json::to_value(catalog).expect("focused catalog serializes for canonical input"),
    )
    .expect("focused catalog canonicalizes");
    assert_eq!(
        FocusedLiveSuccessorCatalogV1::parse_canonical(&canonical)
            .expect("valid canonical focused catalog parses"),
        catalog.clone()
    );

    let pretty = serde_json::to_vec_pretty(catalog).expect("focused catalog pretty-serializes");
    assert_eq!(
        FocusedLiveSuccessorCatalogV1::parse_canonical(&pretty),
        Err(ContractError::NonCanonicalJson)
    );

    let mut unknown = serde_json::to_value(catalog).expect("focused catalog serializes");
    unknown
        .as_object_mut()
        .expect("focused catalog is an object")
        .insert("unexpected".to_owned(), Value::Bool(true));
    let unknown = canonical_jcs(&unknown).expect("unknown-field catalog canonicalizes");
    assert!(matches!(
        FocusedLiveSuccessorCatalogV1::parse_canonical(&unknown),
        Err(ContractError::InvalidJson(_))
    ));

    let mut invalid = catalog.clone();
    invalid.semantic_sha256 = digest(0xff);
    let invalid =
        canonical_jcs(&serde_json::to_value(&invalid).expect("invalid focused catalog serializes"))
            .expect("invalid focused catalog canonicalizes");
    assert!(matches!(
        FocusedLiveSuccessorCatalogV1::parse_canonical(&invalid),
        Err(ContractError::InvalidContract(_))
    ));
}

#[test]
fn shared_catalog_vectors_match_the_closed_schema() {
    let vectors: Value = serde_json::from_slice(include_bytes!(
        "../../../scripts/fixtures/focused_live_successor_catalog_v1_vectors.json"
    ))
    .expect("focused catalog vectors parse as JSON");
    let catalog_schema: Value = serde_json::from_slice(include_bytes!(
        "../../../.codex/validation/focused-live-successor-catalog-v1.schema.json"
    ))
    .expect("focused catalog schema parses");
    let referenced_schemas = [
        include_bytes!("../../../.codex/validation/inventory-shared-types-v1.schema.json")
            .as_slice(),
        include_bytes!("../../../.codex/validation/frozen-test-inventory-v2.schema.json")
            .as_slice(),
    ]
    .into_iter()
    .map(|bytes| {
        let schema: Value = serde_json::from_slice(bytes).expect("referenced schema parses");
        let id = schema["$id"]
            .as_str()
            .expect("referenced schema has an ID")
            .to_owned();
        (id, schema)
    })
    .collect::<Vec<_>>();
    let mut registry = jsonschema::Registry::new();
    for (id, schema) in &referenced_schemas {
        registry = registry
            .add(id.as_str(), schema)
            .expect("referenced schema registers");
    }
    let registry = registry.prepare().expect("schema registry prepares");
    let validator = jsonschema::options()
        .with_registry(&registry)
        .build(&catalog_schema)
        .expect("focused catalog schema compiles");
    for vector in vectors["valid_vectors"]
        .as_array()
        .expect("valid vectors are an array")
    {
        validator
            .validate(&vector["catalog"])
            .unwrap_or_else(|error| panic!("schema rejected {}: {error}", vector["case"]));
    }
}

#[test]
fn empty_repository_catalog_and_zero_volume_identity_are_valid() {
    let (processes, authority, attempt_bounds) = process_evidence();
    let catalog = empty_catalog(&processes);
    catalog.validate().expect("empty catalog validates");
    catalog
        .validate_semantics(
            &catalog.frozen_inventory_hash,
            &catalog.start_fingerprint,
            catalog.start_mutation_epoch,
            0,
            &[],
            &[],
            &[],
            &[],
            &[],
            &catalog.replacement_successor_catalog,
            &catalog.in_process_jest_discovery,
            &[],
            &processes,
            &authority,
            &attempt_bounds,
        )
        .expect("empty catalog closes over trusted inputs");
}

#[test]
fn file_identity_sentinels_are_rejected_independently() {
    let (processes, _, _) = process_evidence();
    for file_id_hex in ["0".repeat(32), "f".repeat(32)] {
        let mut invalid = processes.clone();
        let InventoryDiscoveryOutputV1::ReportFile {
            report_identity, ..
        } = &mut invalid[2].output
        else {
            panic!("root unittest uses a report file");
        };
        report_identity.file_id_hex = file_id_hex;
        assert!(validate_inventory_discovery_process_set_v1(&invalid).is_err());
    }
}

#[test]
fn noncanonical_windows_paths_are_rejected() {
    let (processes, _, _) = process_evidence();
    let invalid_paths = [
        r"\\server\share\work",
        r"C:/work",
        r"C:\work\.\child",
        r"C:\work\..\child",
        r"C:\work\\child",
        "C:\\work\\trailing.\\child",
        "C:\\work\\trailing \\child",
        r"C:\work\name:stream",
        r#"C:\work\bad<name"#,
        r"C:\work\CONIN$",
        r"C:\work\CONOUT$.log",
        r"C:\work\CLOCK$",
    ];
    for path in invalid_paths {
        let mut invalid = processes.clone();
        invalid[0].cwd = path.to_owned();
        assert!(
            validate_inventory_discovery_process_set_v1(&invalid).is_err(),
            "accepted invalid Windows path {path:?}"
        );
    }
}

#[test]
fn bad_report_path_is_rejected_even_when_argv_hash_and_authority_match() {
    let (mut processes, mut authority, attempt_bounds) = process_evidence();
    let bad_path = r"C:\attempt\CLOCK$.json".to_owned();
    let InventoryDiscoveryOutputV1::ReportFile { report_path, .. } = &mut processes[2].output
    else {
        panic!("root unittest uses a report file");
    };
    *report_path = bad_path.clone();
    let output_index = processes[2]
        .argv
        .iter()
        .position(|argument| argument == "--output")
        .expect("report argv has --output");
    processes[2].argv[output_index + 1] = bad_path.clone();
    processes[2].child_process.args_hash = raw_sha256(&processes[2].argv);
    authority.expected_processes[2].argv = processes[2].argv.clone();
    authority.expected_processes[2].report_path = Some(bad_path);
    assert!(
        validate_inventory_discovery_process_authority_v1(&processes, &authority, &attempt_bounds,)
            .is_err()
    );
}
