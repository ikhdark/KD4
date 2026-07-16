use super::*;
use crate::model::RawPath;
use crate::model::RepositorySnapshot;
use crate::model::SnapshotRecord;
use crate::model::SnapshotSource;

#[test]
fn rust_change_uses_conservative_reverse_workspace_closure() {
    let fixture = fixture_repository(false);
    let artifact = decision(fixture.path(), ["codex-rs/a/src/lib.rs"]);
    assert!(!artifact.body.full_fallback);
    assert_eq!(artifact.body.affected_packages, vec!["a"]);
    assert_eq!(artifact.body.reverse_closure, vec!["a", "b"]);
    assert_eq!(artifact.outputs.workflow("rust-ci"), Some(true));
    assert_eq!(artifact.outputs.workflow("cargo-deny"), Some(true));
    assert_eq!(artifact.outputs.workflow("sdk"), Some(false));
}

#[test]
fn cargo_metadata_graph_drives_globbed_conservative_reverse_closure() {
    let fixture = tempfile::tempdir().expect("tempdir");
    write(
        fixture.path().join("codex-rs/Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
    );
    for name in ["a", "b", "c"] {
        write(
            fixture
                .path()
                .join(format!("codex-rs/crates/{name}/Cargo.toml")),
            &format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
            ),
        );
        write(
            fixture
                .path()
                .join(format!("codex-rs/crates/{name}/src/lib.rs")),
            "pub fn owner() {}\n",
        );
    }
    write(
        fixture.path().join("scripts/verify_local_rules.toml"),
        "# fixture rules\n",
    );
    let workspace_root =
        fs::canonicalize(fixture.path().join("codex-rs")).expect("workspace root");
    let package = |name: &str, dependencies: serde_json::Value| {
        serde_json::json!({
            "id": format!("workspace#{name}"),
            "name": name,
            "manifest_path": fs::canonicalize(
                fixture.path().join(format!("codex-rs/crates/{name}/Cargo.toml"))
            ).expect("manifest"),
            "dependencies": dependencies,
        })
    };
    let metadata = serde_json::json!({
        "workspace_root": workspace_root,
        "workspace_members": ["workspace#a", "workspace#b", "workspace#c"],
        "packages": [
            package("a", serde_json::json!([])),
            package("b", serde_json::json!([
                {"name": "a", "kind": null, "optional": true, "rename": "alias_a", "target": "cfg(unix)"},
                {"name": "a", "kind": "build", "optional": false},
                {"name": "a", "kind": "dev", "optional": false}
            ])),
            package("c", serde_json::json!([
                {"name": "b", "kind": null, "optional": false}
            ]))
        ]
    });
    let snapshot = RepositorySnapshot::from_explicit_paths(
        fixture.path(),
        [RawPath::from_utf8("codex-rs/crates/a/src/lib.rs")],
    )
    .expect("snapshot");
    let artifact =
        build_ci_decision_from_metadata(fixture.path(), snapshot, "pull_request", &metadata)
            .expect("decision");
    assert!(!artifact.body.full_fallback);
    assert_eq!(artifact.body.reverse_closure, vec!["a", "b", "c"]);
}

#[test]
fn malformed_metadata_graph_activates_full_fallback() {
    let fixture = fixture_repository(false);
    let artifact = build_ci_decision_from_metadata(
        fixture.path(),
        snapshot(fixture.path(), ["codex-rs/a/src/lib.rs"]),
        "pull_request",
        &serde_json::json!({}),
    )
    .expect("fallback decision");
    assert!(artifact.body.full_fallback);
    assert!(
        artifact
            .body
            .fallback_reasons
            .iter()
            .any(|reason| reason.contains("metadata graph"))
    );
}

#[test]
fn sdk_docs_and_mixed_changes_select_expected_fixed_workflows() {
    let fixture = fixture_repository(false);
    let sdk = decision(fixture.path(), ["sdk/python/openai/__init__.py"]);
    assert_eq!(sdk.outputs.workflow("sdk"), Some(true));
    assert_eq!(sdk.outputs.workflow("rust-ci"), Some(false));
    let docs = decision(fixture.path(), ["docs/verification.md"]);
    assert_eq!(docs.outputs.workflow("codespell"), Some(true));
    assert_eq!(docs.outputs.workflow("sdk"), Some(false));
    let mixed = decision(
        fixture.path(),
        ["codex-rs/a/src/lib.rs", "sdk/typescript/src/index.ts"],
    );
    assert_eq!(mixed.outputs.workflow("rust-ci"), Some(true));
    assert_eq!(mixed.outputs.workflow("sdk"), Some(true));
    assert!(!mixed.body.full_fallback);
}

#[test]
fn workflow_manifest_non_utf8_unknown_and_graph_cycle_fail_closed() {
    let fixture = fixture_repository(false);
    for path in [
        ".github/workflows/blocking-ci.yml",
        "codex-rs/a/Cargo.toml",
        "mystery/file.bin",
    ] {
        assert!(
            decision(fixture.path(), [path]).body.full_fallback,
            "{path}"
        );
    }
    let raw = RepositorySnapshot {
        repository_root: Some(fixture.path().to_path_buf()),
        source: SnapshotSource::ExplicitPaths,
        records: vec![SnapshotRecord {
            status: "M".to_string(),
            path: RawPath::new([0xff, b'x']),
            original_path: None,
            staged: true,
            unstaged: false,
            submodule_state: None,
        }],
        complete: true,
        fallback_reasons: Vec::new(),
    };
    assert!(
        build_ci_decision(fixture.path(), raw, "pull_request")
            .expect("raw decision")
            .body
            .full_fallback
    );
    let cycle = fixture_repository(true);
    assert!(
        decision(cycle.path(), ["codex-rs/a/src/lib.rs"])
            .body
            .full_fallback
    );
}

#[test]
fn rename_and_deletion_preserve_both_paths_for_classification() {
    let fixture = fixture_repository(false);
    let snapshot = RepositorySnapshot {
        repository_root: Some(fixture.path().to_path_buf()),
        source: SnapshotSource::ExplicitPaths,
        records: vec![
            SnapshotRecord {
                status: "R100".to_string(),
                path: RawPath::from_utf8("codex-rs/b/src/new.rs"),
                original_path: Some(RawPath::from_utf8("codex-rs/a/src/old.rs")),
                staged: true,
                unstaged: false,
                submodule_state: None,
            },
            SnapshotRecord {
                status: "D".to_string(),
                path: RawPath::from_utf8("sdk/python/deleted.py"),
                original_path: None,
                staged: true,
                unstaged: false,
                submodule_state: None,
            },
        ],
        complete: true,
        fallback_reasons: Vec::new(),
    };
    let artifact = build_ci_decision(fixture.path(), snapshot, "pull_request").expect("decision");
    assert_eq!(artifact.body.affected_packages, vec!["a", "b"]);
    assert_eq!(artifact.outputs.workflow("sdk"), Some(true));
}

#[test]
fn canonical_bytes_ignore_git_emission_order_and_exclude_decision_id() {
    let fixture = fixture_repository(false);
    let first = snapshot(fixture.path(), ["sdk/python/a.py", "codex-rs/a/src/lib.rs"]);
    let mut second = first.clone();
    second.records.reverse();
    let first = build_ci_decision(fixture.path(), first, "pull_request").expect("first");
    let second = build_ci_decision(fixture.path(), second, "pull_request").expect("second");
    assert_eq!(first.bytes, second.bytes);
    assert_eq!(first.outputs.decision_id, second.outputs.decision_id);
    assert!(!String::from_utf8_lossy(&first.bytes).contains("decision_id"));
    assert_eq!(decision_id(&first.bytes), first.outputs.decision_id);
}

#[test]
fn consumer_hashes_exact_bytes_before_parsing_and_mutation_fails() {
    let fixture = fixture_repository(false);
    let artifact = decision(fixture.path(), ["docs/readme.md"]);
    let parsed = verify_ci_decision_artifact(&artifact.bytes, &artifact.outputs.decision_id)
        .expect("verified artifact");
    assert_eq!(parsed, artifact.body);
    let mut mutated = artifact.bytes.clone();
    mutated.push(b' ');
    assert!(matches!(
        verify_ci_decision_artifact(&mutated, &artifact.outputs.decision_id),
        Err(CiDecisionError::HashMismatch { .. })
    ));
}

#[test]
fn output_budget_fallback_builds_and_hashes_a_new_small_body() {
    let fixture = fixture_repository(false);
    let mut context = PlannerContext::load(fixture.path()).expect("context");
    context
        .graph
        .reverse_dependencies
        .entry("a".to_string())
        .or_default()
        .extend((0..129).map(|index| format!("package-{index:03}")));
    let artifact = build_ci_decision_with_context(
        &context,
        snapshot(fixture.path(), ["codex-rs/a/src/lib.rs"]),
        "pull_request".to_string(),
    )
    .expect("bounded decision");
    assert!(artifact.body.full_fallback);
    assert_eq!(artifact.body.matrix.rust_shards, vec!["workspace"]);
    assert!(artifact.body.matrix.rust_packages.is_empty());
    assert!(artifact.body.changes.is_empty());
    assert!(artifact.body.affected_packages.is_empty());
    assert!(artifact.body.reverse_closure.is_empty());
    assert_eq!(
        artifact.body.fallback_reasons,
        vec!["GitHub output budget exceeded"]
    );
    assert_eq!(decision_id(&artifact.bytes), artifact.outputs.decision_id);
}

#[test]
fn strict_consumer_and_writer_reject_contract_or_identity_mutation() {
    let fixture = fixture_repository(false);
    let artifact = decision(fixture.path(), ["docs/readme.md"]);
    let mut value: serde_json::Value =
        serde_json::from_slice(&artifact.bytes).expect("decision json");
    value
        .as_object_mut()
        .expect("body")
        .insert("decision_id".to_string(), serde_json::json!("forbidden"));
    let mut bytes = serde_json::to_vec_pretty(&value).expect("mutated bytes");
    bytes.push(b'\n');
    assert!(verify_ci_decision_artifact(&bytes, &decision_id(&bytes)).is_err());

    let mut inconsistent = artifact.clone();
    inconsistent.outputs.decision_id = "sha256:".to_string() + &"0".repeat(64);
    let temp = tempfile::tempdir().expect("tempdir");
    assert!(write_ci_decision_artifact(&inconsistent, &temp.path().join("decision.json")).is_err());
}

#[test]
fn snapshot_failure_becomes_a_small_full_suite_decision() {
    let fixture = fixture_repository(false);
    let snapshot = RepositorySnapshot::full_fallback(fixture.path(), "missing shallow history");
    let artifact =
        build_ci_decision(fixture.path(), snapshot, "pull_request").expect("fallback decision");
    assert!(artifact.body.full_fallback);
    assert!(artifact.body.workflows.iter().all(|workflow| workflow.run));
    assert_eq!(artifact.body.matrix.rust_shards, vec!["workspace"]);
}

fn decision<const N: usize>(repository: &Path, paths: [&str; N]) -> CiDecisionArtifact {
    build_ci_decision(repository, snapshot(repository, paths), "pull_request").expect("decision")
}

fn snapshot<const N: usize>(repository: &Path, paths: [&str; N]) -> RepositorySnapshot {
    RepositorySnapshot::from_explicit_paths(repository, paths.into_iter().map(RawPath::from_utf8))
        .expect("snapshot")
}

fn fixture_repository(cycle: bool) -> tempfile::TempDir {
    let fixture = tempfile::tempdir().expect("tempdir");
    write(
        fixture.path().join("codex-rs/Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n[workspace.dependencies]\na = { path = \"a\" }\n",
    );
    let a_dependency = if cycle {
        "[dev-dependencies]\nb = { path = \"../b\" }\n"
    } else {
        ""
    };
    write(
        fixture.path().join("codex-rs/a/Cargo.toml"),
        &format!(
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{a_dependency}"
        ),
    );
    write(
        fixture.path().join("codex-rs/a/src/lib.rs"),
        "pub fn a() {}\n",
    );
    write(
        fixture.path().join("codex-rs/b/Cargo.toml"),
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\na = { workspace = true, optional = true }\n[target.'cfg(windows)'.build-dependencies]\na = { path = \"../a\" }\n[dev-dependencies]\na = { path = \"../a\" }\n",
    );
    write(
        fixture.path().join("codex-rs/b/src/lib.rs"),
        "pub fn b() {}\n",
    );
    write(
        fixture.path().join("scripts/verify_local_rules.toml"),
        "# fixture rules\n",
    );
    fixture
}

fn write(path: PathBuf, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}
