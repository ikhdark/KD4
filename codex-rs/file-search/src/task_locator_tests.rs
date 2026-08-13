use super::*;
use pretty_assertions::assert_eq;
use std::fs;
use tempfile::TempDir;

#[test]
fn embedded_parsers_normalize_rust_and_typescript_constructs() {
    let rust = br#"
use crate::worker::run;
pub struct Engine;
impl Engine {
    pub fn dispatch() { run(); }
}
#[test]
fn dispatches() { Engine::dispatch(); }
"#;
    let parsed = parse_source_file("src/lib.rs", rust);
    assert!(parsed.definitions.iter().any(|item| item.name == "Engine"));
    assert!(
        parsed
            .definitions
            .iter()
            .any(|item| item.name == "dispatch")
    );
    assert!(
        parsed
            .calls
            .iter()
            .any(|item| item.callee == "run" && item.direct)
    );
    assert!(parsed.tests.iter().any(|item| item.name == "dispatches"));
    assert!(
        parsed
            .module_edges
            .iter()
            .any(|item| item.kind == "rust_use")
    );

    let typescript = br#"
import { locate } from "./locator";
export const run = (task: string) => locate(task);
class Runner { execute() { return run("x"); } }
test("runs", () => run("x"));
"#;
    let parsed = parse_source_file("src/index.tsx", typescript);
    assert_eq!(parsed.language, "tsx");
    assert!(parsed.definitions.iter().any(|item| item.name == "run"));
    assert!(parsed.definitions.iter().any(|item| item.name == "Runner"));
    assert!(parsed.imports.iter().any(|item| item.source == "./locator"));
    assert!(parsed.tests.iter().any(|item| item.name == "runs"));
}

#[test]
fn parser_failures_are_file_local_diagnostics() {
    let parsed = parse_source_file("broken.rs", b"fn broken( {");
    assert!(!parsed.diagnostics.is_empty());
    let valid = parse_source_file("valid.rs", b"fn valid() {}\n");
    assert!(valid.definitions.iter().any(|item| item.name == "valid"));
}

#[test]
fn manifest_validation_rejects_a_missing_primary_entry_symbol() {
    let fixture = Fixture::new();
    let mut manifest = fixture.manifest.clone();
    manifest.owners[0].primary_entries[0].symbol = "not_in_this_file".to_string();
    fs::write(
        &fixture.manifest_path,
        toml::to_string(&manifest).expect("toml"),
    )
    .expect("manifest");

    let validation = validate_routing_manifest(fixture.root.path(), &fixture.manifest_path);
    assert!(validation.errors.iter().any(|error| {
        error.contains("routing_manifest_invalid") && error.contains("not_in_this_file")
    }));
}

#[test]
fn invalid_manifest_cannot_authoritatively_route() {
    let fixture = Fixture::new();
    let mut manifest = fixture.manifest.clone();
    manifest.owners[0].primary_entries[0].symbol = "missing_entry".to_string();
    fs::write(
        &fixture.manifest_path,
        toml::to_string(&manifest).expect("manifest"),
    )
    .expect("write invalid manifest");
    let output = locate_task(&fixture.request("change alpha locator evidence", None, None, true))
        .expect("locate");
    let value: serde_json::Value = serde_json::from_str(&output.rendered).expect("json");
    assert!(value["routing"]["owner_id"].is_null());
    assert_ne!(value["routing"]["provenance"], "manifest_declared");
    assert!(
        value["unresolved"]
            .as_array()
            .expect("unresolved")
            .iter()
            .any(|item| item
                .as_str()
                .is_some_and(|item| item.contains("routing_manifest_invalid")))
    );
}

#[test]
fn caller_test_generated_and_wrapper_candidates_resolve_to_the_real_primary_owner() {
    for (relation, candidate) in [
        ("caller", "src/callers/alpha_caller.rs"),
        ("test", "tests/alpha_tests.rs"),
        ("generated", "schema/alpha.generated.json"),
        ("wrapper", "wrappers/alpha_wrapper.rs"),
    ] {
        let fixture = Fixture::new();
        let candidate_path = fixture.root.path().join(candidate);
        fs::create_dir_all(candidate_path.parent().expect("candidate parent"))
            .expect("candidate directory");
        fs::write(&candidate_path, "alpha locator evidence\n").expect("candidate source");
        let mut manifest = fixture.manifest.clone();
        match relation {
            "caller" => manifest.owners[0].consumers.push(candidate.to_string()),
            "test" => manifest.owners[0].tests.push(candidate.to_string()),
            "generated" => manifest.owners[0]
                .generated_mirrors
                .push(candidate.to_string()),
            "wrapper" => manifest.owners[0].contracts.push(candidate.to_string()),
            _ => unreachable!("bounded relation table"),
        }
        fs::write(
            &fixture.manifest_path,
            toml::to_string(&manifest).expect("toml"),
        )
        .expect("manifest");

        let resolution = resolve_owner_candidates(
            fixture.root.path(),
            &fixture.manifest_path,
            &[candidate.to_string()],
        );

        let owner = resolution
            .authoritative_owner
            .unwrap_or_else(|| panic!("validated {relation} mapping should resolve"));
        assert_eq!(owner.id, "alpha");
        assert_eq!(owner.primary_entries[0].path, "src/alpha/lib.rs");
        assert_ne!(owner.primary_entries[0].path, candidate);
    }
}

#[test]
fn candidates_mapping_to_multiple_valid_owners_remain_unresolved() {
    let fixture = Fixture::new();
    let resolution = resolve_owner_candidates(
        fixture.root.path(),
        &fixture.manifest_path,
        &[
            "src/alpha/lib.rs".to_string(),
            "src/beta/lib.ts".to_string(),
        ],
    );

    assert!(resolution.authoritative_owner.is_none());
    assert_eq!(
        resolution
            .alternative_owners
            .iter()
            .map(|owner| owner.id.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "beta"]
    );
    assert!(
        resolution
            .unresolved
            .iter()
            .any(|reason| reason == "candidate_owner_conflict")
    );
}

#[test]
fn routing_enforces_anchor_conflicts_and_margin() {
    let manifest = RoutingManifest {
        schema_version: 1,
        owners: vec![
            owner(
                "alpha",
                "src/alpha",
                "alpha_entry",
                "alpha locator evidence",
            ),
            owner("beta", "src/beta", "beta_entry", "beta protocol routing"),
        ],
    };
    let conflict = route_task(
        Some(&manifest),
        "alpha locator evidence",
        Some("src/alpha/lib.rs"),
        Some("beta_entry"),
    );
    assert_eq!(conflict.status, "anchor_conflict");
    let selected = route_task(
        Some(&manifest),
        "please change alpha locator evidence",
        None,
        None,
    );
    assert_eq!(selected.owner.map(|owner| owner.id.as_str()), Some("alpha"));
    let ambiguous = route_task(Some(&manifest), "routing", None, None);
    assert_eq!(ambiguous.status, "owner_ambiguity");
}

#[test]
fn exact_syntax_symbol_anchor_routes_beyond_manifest_entry_symbols() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.path().join("src/beta/helper.ts"),
        "export function locate_hidden_beta() { return true; }\n",
    )
    .expect("helper");

    let output = locate_task(&fixture.request(
        "alpha locator evidence",
        None,
        Some("locate_hidden_beta"),
        false,
    ))
    .expect("locate");
    let value: serde_json::Value = serde_json::from_str(&output.rendered).expect("json");
    assert_eq!(value["routing"]["owner_id"], "beta");
    assert_eq!(value["routing"]["provenance"], "anchor_exact");
    assert_eq!(value["primary"]["symbol"], "locate_hidden_beta");
}

#[test]
fn duplicate_exact_syntax_definition_is_ambiguous() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.path().join("src/alpha/duplicate.rs"),
        "pub fn duplicate_anchor() {}\n",
    )
    .expect("alpha duplicate");
    fs::write(
        fixture.root.path().join("src/beta/duplicate.ts"),
        "export function duplicate_anchor() {}\n",
    )
    .expect("beta duplicate");

    let output = locate_task(&fixture.request(
        "alpha locator evidence",
        None,
        Some("duplicate_anchor"),
        false,
    ))
    .expect("locate");
    let value: serde_json::Value = serde_json::from_str(&output.rendered).expect("json");
    assert_eq!(value["routing"]["status"], "symbol_ambiguity");
    let unresolved = value["unresolved"].as_array().expect("unresolved");
    assert!(unresolved.iter().any(|item| {
        item.as_str()
            .is_some_and(|item| item.contains("src/alpha/duplicate.rs"))
    }));
    assert!(unresolved.iter().any(|item| {
        item.as_str()
            .is_some_and(|item| item.contains("src/beta/duplicate.ts"))
    }));
}

#[test]
fn locate_task_is_warm_deterministic_bounded_and_closure_scoped() {
    let fixture = Fixture::new();
    let request = fixture.request("change alpha locator evidence", None, None, false);
    let cold = locate_task(&request).expect("cold locate");
    assert!(cold.rendered.len() <= LOCATE_TASK_MAX_RENDERED_BYTES);
    let first = locate_task(&request).expect("first warm locate");
    let second = locate_task(&request).expect("second warm locate");
    assert_eq!(first.rendered, second.rendered);
    let value: serde_json::Value = serde_json::from_str(&first.rendered).expect("valid json");
    for field in [
        "schema_version",
        "environment",
        "repository",
        "snapshot",
        "routing",
        "primary",
        "source_neighborhoods",
        "instructions",
        "relationships",
        "contracts",
        "tests",
        "validation",
        "alternatives",
        "unresolved",
        "truncation",
        "followups",
    ] {
        assert!(value.get(field).is_some(), "missing {field}");
    }
    assert_eq!(value["routing"]["owner_id"], "alpha");
    assert_eq!(value["primary"]["symbol"], "locate_alpha");
    assert!(!cold.supporting_reads.is_empty());
    assert!(cold.files_reparsed > 0);
    assert_eq!(first.files_reparsed, 0);
    assert_eq!(second.files_reparsed, 0);
    assert_eq!(second.rendered_bytes, second.rendered.len());
}

#[test]
fn locate_task_bundles_primary_caller_focused_test_and_contract_in_owner_order() {
    let fixture = Fixture::new();
    let caller = "src/alpha/caller.rs";
    let focused_test = "src/alpha/focused_test.rs";
    let contract = "src/alpha/contract.md";
    fs::write(
        fixture.root.path().join(caller),
        "pub fn call_alpha() { crate::locate_alpha(\"caller\"); }\n",
    )
    .expect("caller");
    fs::write(
        fixture.root.path().join(focused_test),
        "#[test]\nfn focused_alpha() { assert!(!crate::locate_alpha(\"test\")); }\n",
    )
    .expect("focused test");
    fs::write(
        fixture.root.path().join(contract),
        "# Alpha contract\nThe locator accepts a task string.\n",
    )
    .expect("contract");
    let mut manifest = fixture.manifest.clone();
    manifest.owners[0].consumers.push(caller.to_string());
    manifest.owners[0].tests.push(focused_test.to_string());
    manifest.owners[0].contracts.push(contract.to_string());
    fs::write(
        &fixture.manifest_path,
        toml::to_string(&manifest).expect("manifest"),
    )
    .expect("manifest");

    let output = locate_task(&fixture.request("change alpha locator evidence", None, None, true))
        .expect("locate bundled evidence");
    assert!(output.rendered.len() <= LOCATE_TASK_MAX_RENDERED_BYTES);
    let sections = &output.decision_facts.captured_source_sections;
    let position = |kind, path: &str| {
        sections
            .iter()
            .position(|section| section.kind == kind && section.path == path)
            .unwrap_or_else(|| panic!("missing {kind:?} section for {path}"))
    };
    let primary_position = position(
        LocateTaskSourceSectionKind::PrimaryImplementation,
        "src/alpha/lib.rs",
    );
    let caller_position = position(LocateTaskSourceSectionKind::Caller, caller);
    let test_position = position(LocateTaskSourceSectionKind::Test, focused_test);
    let contract_position = position(LocateTaskSourceSectionKind::Contract, contract);
    assert!(primary_position < caller_position);
    assert!(caller_position < test_position);
    assert!(test_position < contract_position);
    for path in ["src/alpha/lib.rs", caller, focused_test, contract] {
        assert!(sections.iter().any(|section| {
            section.path == path
                && section.state == LocateTaskSourceSectionState::Materialized
                && section.text.is_some()
        }));
    }
}

#[test]
fn locator_decision_facts_are_neutral_source_closure_evidence() {
    let fixture = Fixture::new();
    let output = locate_task(&fixture.request("change alpha locator evidence", None, None, true))
        .expect("locate");
    let owner = output
        .decision_facts
        .authoritative_owner
        .as_ref()
        .expect("validated raw owner declaration");
    assert_eq!(owner.id, "alpha");
    assert_eq!(owner.primary_entries[0].path, "src/alpha/lib.rs");
    let packet_seed = output
        .owner_packet_seed
        .as_ref()
        .expect("bounded owner packet seed");
    assert_eq!(packet_seed.authoritative_owner.id, "alpha");
    assert!(!packet_seed.supporting_reads.is_empty());
    let packet_seed_json = serde_json::to_string(packet_seed).expect("serialize packet seed");
    assert!(!packet_seed_json.contains("fn locate_alpha"));

    let encoded = serde_json::to_string(&output.decision_facts).expect("serialize facts");
    for forbidden in [
        "lifecycle_obligations",
        "multi_owner_atomic_evidence",
        "generated_source_ownership",
        "validation_disposition",
        "applicability",
        "receipt_id",
        "closure_state",
        "artifact_line_range",
        "projection",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "unexpected `{forbidden}` in {encoded}"
        );
    }
}

#[test]
fn instructions_bypass_closure_caps_and_remain_snapshot_contributors() {
    let fixture = Fixture::new();
    let nested_instructions = fixture.root.path().join("src/alpha/AGENTS.md");
    let initial_bytes = b"Use nested source validation.\n";
    fs::write(&nested_instructions, initial_bytes).expect("nested instructions");
    let mut request = fixture.request("change alpha locator evidence", None, None, true);
    request.max_files = 1;

    let first = locate_task(&request).expect("first locate");
    let value: serde_json::Value = serde_json::from_str(&first.rendered).expect("json");
    let instruction_paths = value["instructions"]
        .as_array()
        .expect("instructions")
        .iter()
        .filter_map(|instruction| instruction["path"].as_str())
        .collect::<Vec<_>>();
    assert!(instruction_paths.contains(&"AGENTS.md"));
    assert!(instruction_paths.contains(&"src/alpha/AGENTS.md"));
    assert_eq!(
        first
            .supporting_reads
            .iter()
            .find(|read| read.path == "src/alpha/AGENTS.md")
            .map(|read| read.content_hash.as_str()),
        Some(sha256_bytes(initial_bytes).as_str())
    );
    assert_eq!(
        value["snapshot"]["contributing_file_count"],
        first.supporting_reads.len()
    );

    fs::write(
        &nested_instructions,
        b"Use nested generated source validation.\n",
    )
    .expect("update nested instructions");
    let second = locate_task(&request).expect("second locate");
    assert_ne!(first.snapshot_id, second.snapshot_id);
}

#[test]
fn routing_manifest_is_a_supporting_snapshot_contributor() {
    let fixture = Fixture::new();
    let request = fixture.request("change alpha locator evidence", None, None, true);
    let first = locate_task(&request).expect("first locate");
    let initial_manifest = fs::read(&fixture.manifest_path).expect("manifest bytes");
    assert_eq!(
        first
            .supporting_reads
            .iter()
            .find(|read| read.path == "source_owners.toml")
            .map(|read| read.content_hash.as_str()),
        Some(sha256_bytes(&initial_manifest).as_str())
    );

    let mut updated_manifest = fixture.manifest.clone();
    updated_manifest.owners[0]
        .aliases
        .push("alpha-updated".to_string());
    fs::write(
        &fixture.manifest_path,
        toml::to_string(&updated_manifest).expect("toml"),
    )
    .expect("updated manifest");
    let second = locate_task(&request).expect("second locate");
    assert_ne!(first.snapshot_id, second.snapshot_id);
    assert!(
        second
            .supporting_reads
            .iter()
            .any(|read| read.path == "source_owners.toml")
    );
}

#[test]
fn missing_manifest_created_during_query_is_retried_and_captured() {
    let fixture = Fixture::new();
    let manifest_bytes = toml::to_string(&fixture.manifest)
        .expect("toml")
        .into_bytes();
    fs::remove_file(&fixture.manifest_path).expect("remove manifest");
    let manifest_path = fixture.manifest_path.clone();
    let hook_bytes = manifest_bytes.clone();
    install_before_final_verify_hook(fixture.root.path(), move || {
        fs::write(manifest_path, hook_bytes).expect("restore manifest");
    });

    let output = locate_task(&fixture.request("change alpha locator evidence", None, None, true))
        .expect("locate after manifest retry");
    let value: serde_json::Value = serde_json::from_str(&output.rendered).expect("json");
    assert_eq!(value["routing"]["owner_id"], "alpha");
    assert_eq!(
        output
            .supporting_reads
            .iter()
            .find(|read| read.path == "source_owners.toml")
            .map(|read| read.content_hash.as_str()),
        Some(sha256_bytes(&manifest_bytes).as_str())
    );
}

#[test]
fn caller_change_during_capture_retries_once_and_returns_one_stable_snapshot() {
    let fixture = Fixture::new();
    let caller_path = "src/alpha/caller.rs";
    fs::write(
        fixture.root.path().join(caller_path),
        "pub fn call() { crate::locate_alpha(\"old\"); }\n",
    )
    .expect("caller");
    let mut manifest = fixture.manifest.clone();
    manifest.owners[0].consumers.push(caller_path.to_string());
    fs::write(
        &fixture.manifest_path,
        toml::to_string(&manifest).expect("manifest"),
    )
    .expect("manifest");
    let changed_path = fixture.root.path().join(caller_path);
    install_before_final_verify_hook(fixture.root.path(), move || {
        fs::write(
            changed_path,
            "pub fn call() { crate::locate_alpha(\"new\"); }\n",
        )
        .expect("change caller");
    });

    let output = locate_task(&fixture.request("change alpha locator evidence", None, None, true))
        .expect("stable retry");

    assert_eq!(
        output.decision_facts.selected_owner.as_deref(),
        Some("alpha")
    );
    assert!(
        !output
            .decision_facts
            .source_gaps
            .contains(&"source_changed".to_string())
    );
    assert!(
        output
            .decision_facts
            .captured_source_sections
            .iter()
            .all(|section| {
                section.source_snapshot_identity == output.decision_facts.source_snapshot_identity
            })
    );
    assert!(
        output
            .decision_facts
            .captured_source_sections
            .iter()
            .filter(|section| section.path == caller_path)
            .any(|section| section
                .text
                .as_deref()
                .is_some_and(|text| text.contains("new")))
    );
}

#[test]
fn second_source_change_returns_owner_with_incomplete_unmixed_sections() {
    let fixture = Fixture::new();
    let caller_path = "src/alpha/caller.rs";
    fs::write(
        fixture.root.path().join(caller_path),
        "pub fn call() { crate::locate_alpha(\"first\"); }\n",
    )
    .expect("caller");
    let mut manifest = fixture.manifest.clone();
    manifest.owners[0].consumers.push(caller_path.to_string());
    fs::write(
        &fixture.manifest_path,
        toml::to_string(&manifest).expect("manifest"),
    )
    .expect("manifest");
    let root = fixture.root.path().to_path_buf();
    let first_path = root.join(caller_path);
    let second_root = root.clone();
    install_before_final_verify_hook(&root, move || {
        fs::write(
            first_path,
            "pub fn call() { crate::locate_alpha(\"second\"); }\n",
        )
        .expect("first change");
        let second_path = second_root.join(caller_path);
        install_before_final_verify_hook(&second_root, move || {
            fs::write(
                second_path,
                "pub fn call() { crate::locate_alpha(\"third\"); }\n",
            )
            .expect("second change");
        });
    });

    let output = locate_task(&fixture.request("change alpha locator evidence", None, None, true))
        .expect("bounded unstable result");

    assert_eq!(
        output.decision_facts.selected_owner.as_deref(),
        Some("alpha")
    );
    assert_eq!(output.decision_facts.completeness, "partial");
    assert!(
        output
            .decision_facts
            .source_gaps
            .iter()
            .any(|gap| gap == "source_changed")
    );
    assert!(
        output
            .decision_facts
            .captured_source_sections
            .iter()
            .all(|section| {
                section.source_snapshot_identity == output.decision_facts.source_snapshot_identity
                    && section.state == LocateTaskSourceSectionState::NotMaterialized
                    && section.text.is_none()
                    && section.content_hash.is_none()
            })
    );
}

#[test]
fn locate_task_marks_file_local_parse_diagnostics_partial() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.path().join("src/alpha/broken.rs"),
        "pub fn broken( {\n",
    )
    .expect("broken source");
    let output = locate_task(&fixture.request("change alpha locator evidence", None, None, true))
        .expect("locate");
    let value: serde_json::Value = serde_json::from_str(&output.rendered).expect("json");
    assert_eq!(value["snapshot"]["completeness"], "partial");
    assert!(
        value["unresolved"]
            .as_array()
            .expect("unresolved")
            .iter()
            .any(|item| item
                .as_str()
                .is_some_and(|item| item.contains("parse_diagnostic:src/alpha/broken.rs")))
    );
}

#[test]
fn closure_overflow_admits_no_arbitrary_directory_prefix() {
    let fixture = Fixture::new();
    let owner = fixture.manifest.owners.first().expect("owner");
    let (paths, omitted) = closure_paths(
        fixture.root.path(),
        Some(&fixture.manifest),
        Some(owner),
        None,
        false,
        1,
        64,
    )
    .expect("closure");
    assert!(paths.iter().all(|path| path == "AGENTS.md"));
    assert!(omitted.iter().any(|path| path == "src/alpha"));
}

#[test]
fn corrupt_cache_fails_open_and_is_quarantined() {
    let fixture = Fixture::new();
    let canonical_root = fixture.root.path().canonicalize().expect("canonical root");
    let cache_path = cache_path(fixture.cache.path(), &canonical_root);
    fs::create_dir_all(cache_path.parent().expect("parent")).expect("cache dir");
    fs::write(&cache_path, b"not-json").expect("corrupt cache");
    let output = locate_task(&fixture.request("alpha locator evidence", None, None, false))
        .expect("fail open");
    assert!(output.rendered.contains("locate_alpha"));
    assert!(
        cache_path
            .parent()
            .expect("parent")
            .read_dir()
            .expect("read cache")
            .any(|entry| {
                entry
                    .ok()
                    .and_then(|entry| entry.file_name().to_str().map(str::to_string))
                    .is_some_and(|name| name.contains("corrupt"))
            })
    );
}

fn owner(id: &str, root: &str, symbol: &str, phrase: &str) -> OwnerDeclaration {
    OwnerDeclaration {
        id: id.to_string(),
        concern_ids: Vec::new(),
        aliases: vec![id.to_string()],
        phrases: vec![phrase.to_string()],
        ambiguous_with: Vec::new(),
        roots: vec![root.to_string()],
        primary_entries: vec![EntryDeclaration {
            path: format!("{root}/lib.rs"),
            symbol: symbol.to_string(),
            ambiguous: false,
        }],
        instructions: vec!["AGENTS.md".to_string()],
        consumers: Vec::new(),
        contracts: Vec::new(),
        generated_mirrors: Vec::new(),
        tests: Vec::new(),
        validation: vec![ValidationDeclaration {
            id: format!("{id}-focused"),
            cwd: ".".to_string(),
            argv: vec!["cargo".to_string(), "test".to_string()],
            role: "focused_tests".to_string(),
        }],
    }
}

struct Fixture {
    root: TempDir,
    cache: TempDir,
    manifest_path: PathBuf,
    manifest: RoutingManifest,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("root");
        let cache = tempfile::tempdir().expect("cache");
        fs::create_dir_all(root.path().join("src/alpha")).expect("alpha dir");
        fs::create_dir_all(root.path().join("src/beta")).expect("beta dir");
        fs::write(
            root.path().join("AGENTS.md"),
            "Use focused source validation.\n",
        )
        .expect("agents");
        fs::write(
            root.path().join("src/alpha/lib.rs"),
            "pub fn locate_alpha(task: &str) -> bool { task.is_empty() }\n#[test]\nfn locates() { assert!(!locate_alpha(\"x\")); }\n",
        )
        .expect("alpha");
        fs::write(
            root.path().join("src/beta/lib.ts"),
            "export function beta_entry() { return true; }\n",
        )
        .expect("beta");
        let mut manifest = RoutingManifest {
            schema_version: 1,
            owners: vec![
                owner(
                    "alpha",
                    "src/alpha",
                    "locate_alpha",
                    "alpha locator evidence",
                ),
                owner("beta", "src/beta", "beta_entry", "beta protocol routing"),
            ],
        };
        manifest.owners[1].primary_entries[0].path = "src/beta/lib.ts".to_string();
        let manifest_path = root.path().join("source_owners.toml");
        fs::write(&manifest_path, toml::to_string(&manifest).expect("toml")).expect("manifest");
        let validation = validate_routing_manifest(root.path(), &manifest_path);
        assert!(
            validation.errors.is_empty(),
            "fixture manifest must be valid: {:#?}",
            validation.errors
        );
        Self {
            root,
            cache,
            manifest_path,
            manifest,
        }
    }

    fn request<'a>(
        &'a self,
        task: &'a str,
        path_anchor: Option<&'a str>,
        symbol_anchor: Option<&'a str>,
        force_fresh: bool,
    ) -> LocateTaskRequest<'a> {
        LocateTaskRequest {
            repository_root: self.root.path(),
            cache_root: self.cache.path(),
            manifest_path: &self.manifest_path,
            environment_id: None,
            task,
            path_anchor,
            symbol_anchor,
            max_files: LOCATE_TASK_MAX_FILES,
            max_source_bytes: LOCATE_TASK_MAX_SOURCE_BYTES,
            force_fresh,
        }
    }
}
