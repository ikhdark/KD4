use super::builds;
use super::environment::BASE_CONFIG;
use super::environment::Environment;
use super::feature_overrides;
use super::provenance::FileIdentity;
use super::provenance::git;
use super::provenance::materialize_commit;
use super::provenance::read_json;
use super::provenance::reset_workspace;
use super::provenance::write_json;
use super::provenance::{self};
use super::resolve_source;
use crate::schedule::Variant;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

fn repository(parent: &Path, name: &str, contents: &str) -> PathBuf {
    let root = parent.join(name);
    fs::create_dir_all(root.join("nested")).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch=main"]).unwrap();
    git(&root, &["config", "core.autocrlf", "false"]).unwrap();
    fs::write(root.join("tracked.txt"), contents).unwrap();
    fs::write(root.join("nested/binary.dat"), [0, 255, 4, 13, 10]).unwrap();
    git(&root, &["add", "."]).unwrap();
    git(
        &root,
        &[
            "-c",
            "user.name=Repo Benchmark",
            "-c",
            "user.email=repo-benchmark@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "pinned fixture",
        ],
    )
    .unwrap();
    root
}

#[test]
fn atomic_checkpoint_keeps_the_last_record_until_a_complete_replacement_is_published() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("attempt.json");
    provenance::write_atomic_json(&path, &json!({"status":"running"})).unwrap();
    // Simulate interruption during an unpublished replacement, without a sidecar.
    fs::write(temp.path().join("attempt.interrupted.pending"), b"{partial").unwrap();
    assert_eq!(
        read_json::<Value>(&path).unwrap(),
        json!({"status":"running"})
    );
    assert!(!path.with_extension("sha256").exists());
    provenance::write_atomic_json(&path, &json!({"status":"completed"})).unwrap();
    assert_eq!(
        read_json::<Value>(&path).unwrap(),
        json!({"status":"completed"})
    );
    let changed = fs::read_to_string(&path)
        .unwrap()
        .replace("completed", "incorrect");
    fs::write(&path, changed).unwrap();
    assert!(
        read_json::<Value>(&path)
            .unwrap_err()
            .to_string()
            .contains("changed JSON artifact")
    );
}

#[test]
fn atomic_checkpoint_failed_publication_removes_unpublished_records() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("attempt.json");
    fs::create_dir(&path).unwrap();
    let existing = path.join("existing");
    fs::write(&existing, b"preserve me").unwrap();

    for attempt in 0..3 {
        let error = provenance::write_atomic_json(&path, &json!({"attempt": attempt}))
            .expect_err("a checkpoint cannot replace a directory");
        assert!(error.to_string().contains("atomically publish checkpoint"));
        assert_eq!(fs::read(&existing).unwrap(), b"preserve me");
        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![path.clone()], "failed attempt {attempt}");
    }
}

#[test]
fn atomic_checkpoint_rejects_invalid_payloads_without_changing_the_record() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("attempt.json");
    let accepted = json!({"status": "running"});
    provenance::write_atomic_json(&path, &accepted).unwrap();
    let original_bytes = fs::read(&path).unwrap();

    for (payload, expected_error) in [
        (json!([]), "atomic record must be an object"),
        (
            json!({"_recordSha256": "forged"}),
            "reserved record checksum key",
        ),
    ] {
        let error = provenance::write_atomic_json(&path, &payload).unwrap_err();
        assert_eq!(error.to_string(), expected_error);
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        assert_eq!(read_json::<Value>(&path).unwrap(), accepted);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}

#[test]
fn feature_inventory_comes_from_selected_commit_despite_dirty_inventory() {
    let temp = tempfile::tempdir().unwrap();
    let repo = repository(temp.path(), "source", "source");
    let contents = b"# selected commit\r\nfeatures = []\r\n";
    fs::write(repo.join("kd4_features.toml"), contents).unwrap();
    git(&repo, &["add", "kd4_features.toml"]).unwrap();
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "inventory",
        ],
    )
    .unwrap();
    let source = resolve_source(&repo, "HEAD", temp.path().join("checkout"), false).unwrap();
    fs::write(repo.join("kd4_features.toml"), "features = ['dirty']").unwrap();
    let frozen = temp.path().join("frozen.toml");
    super::snapshot_feature_inventory(&source, &frozen).unwrap();
    assert_eq!(fs::read(frozen).unwrap(), contents);
    assert_eq!(
        fs::read_to_string(repo.join("kd4_features.toml")).unwrap(),
        "features = ['dirty']"
    );
}

#[test]
fn native_checkout_uses_exact_destination_without_origin_registrations() {
    let temp = tempfile::tempdir().unwrap();
    let repo = repository(temp.path(), "source", "pinned native contents\n");
    // A clone does not inherit the source repository's local core.autocrlf.
    // Pin this fixture's checkout bytes independently of the user's Git defaults.
    fs::write(
        repo.join(".gitattributes"),
        "*.txt text eol=lf\n*.snap text eol=lf\n",
    )
    .unwrap();
    let snapshot = PathBuf::from("nested").join(format!("{}.snap", "long-snapshot-name".repeat(8)));
    fs::write(repo.join(&snapshot), "pinned snapshot bytes\n").unwrap();
    // Revisions without eol attributes, like upstream prompt assets, must still
    // build from their committed blobs under a host core.autocrlf=true.
    fs::write(
        repo.join("prompt.md"),
        "embedded line one\nembedded line two\n",
    )
    .unwrap();
    git(&repo, &["add", "."]).unwrap();
    git(
        &repo,
        &[
            "-c",
            "user.name=Repo Benchmark",
            "-c",
            "user.email=repo-benchmark@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "long snapshot",
        ],
    )
    .unwrap();
    git(&repo, &["config", "core.longpaths", "false"]).unwrap();
    let prepared_root = fs::canonicalize(temp.path()).unwrap();
    let destination = prepared_root
        .join("prepared-sources-with-a-fixed-benchmark-owned-location".repeat(2))
        .join("native-checkout");
    let source = resolve_source(&repo, "HEAD", destination.clone(), false).unwrap();
    let original_worktrees = git(&repo, &["worktree", "list", "--porcelain"]).unwrap();
    super::checkout(&source).unwrap();
    assert_eq!(
        git(&repo, &["worktree", "list", "--porcelain"]).unwrap(),
        original_worktrees,
        "native preparation must not register a worktree in the user's repository"
    );
    assert!(
        destination.join(".git").is_dir(),
        "prepared source owns an independent Git directory"
    );
    assert!(
        !destination.join(".git/objects/info/alternates").exists(),
        "prepared objects must not depend on the user's mutable object store"
    );
    assert_eq!(fs::canonicalize(&destination).unwrap(), destination);
    assert_eq!(
        fs::read(destination.join("tracked.txt")).unwrap(),
        b"pinned native contents\n"
    );
    assert!(destination.join(&snapshot).as_os_str().len() > 260);
    assert_eq!(
        fs::read(destination.join(&snapshot)).unwrap(),
        b"pinned snapshot bytes\n"
    );
    assert_eq!(
        fs::read(destination.join("prompt.md")).unwrap(),
        b"embedded line one\nembedded line two\n"
    );
    assert_eq!(
        git(&destination, &["rev-parse", "HEAD"]).unwrap(),
        source.revision
    );
    assert_eq!(git(&repo, &["branch", "--show-current"]).unwrap(), "main");
    assert_eq!(
        git(&repo, &["config", "--local", "--get", "core.longpaths"]).unwrap(),
        "false",
        "preparation must not change the user's repository configuration"
    );
}

#[test]
fn failed_native_checkout_cleans_only_its_new_destination() {
    let temp = tempfile::tempdir().unwrap();
    let repo = repository(temp.path(), "source", "preserved original contents\n");
    let destination = temp.path().join("prepared/native");
    let mut source = resolve_source(&repo, "HEAD", destination.clone(), false).unwrap();
    let original_worktrees = git(&repo, &["worktree", "list", "--porcelain"]).unwrap();
    source.revision = "0".repeat(40);
    let error = super::checkout(&source).unwrap_err();
    assert!(format!("{error:#}").contains("initialize independent local native checkout"));
    assert!(
        !destination.exists(),
        "failed initialization must not leave a partial checkout"
    );
    assert_eq!(
        git(&repo, &["worktree", "list", "--porcelain"]).unwrap(),
        original_worktrees
    );
    assert_eq!(
        fs::read(repo.join("tracked.txt")).unwrap(),
        b"preserved original contents\n"
    );
    fs::create_dir_all(&destination).unwrap();
    fs::write(destination.join("existing-build-evidence"), b"keep").unwrap();
    assert!(
        super::checkout(&source)
            .unwrap_err()
            .to_string()
            .contains("destination already exists")
    );
    assert_eq!(
        fs::read(destination.join("existing-build-evidence")).unwrap(),
        b"keep"
    );
}

#[test]
fn selected_reference_toolchain_uses_commit_and_rejects_different_shared_pin() {
    let temp = tempfile::tempdir().unwrap();
    let repo = repository(temp.path(), "reference", "reference fixture\n");
    fs::create_dir(repo.join("codex-rs")).unwrap();
    let declaration = repo.join("codex-rs/rust-toolchain.toml");
    fs::write(&declaration, "[toolchain]\nchannel = '1.95.0'\n").unwrap();
    git(&repo, &["add", "."]).unwrap();
    git(
        &repo,
        &[
            "-c",
            "user.name=Repo Benchmark",
            "-c",
            "user.email=repo-benchmark@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "pin compiler",
        ],
    )
    .unwrap();
    let source =
        resolve_source(&repo, "HEAD", temp.path().join("prepared/reference"), false).unwrap();
    fs::write(&declaration, "[toolchain]\nchannel = '1.96.0'\n").unwrap();
    super::validate_selected_toolchain(&source, "1.95.0", "reference").unwrap();
    let error = format!(
        "{:#}",
        super::validate_selected_toolchain(&source, "1.94.0", "reference").unwrap_err()
    );
    assert!(error.contains(&format!("reference at {}", source.revision)));
    assert!(error.contains("declares Rust 1.95.0"));
    assert!(error.contains("shared benchmark toolchain is 1.94.0"));
    assert!(
        !source.checkout.exists(),
        "pin mismatch must fail before native checkout or build"
    );
    assert_eq!(
        fs::read_to_string(&declaration).unwrap(),
        "[toolchain]\nchannel = '1.96.0'\n"
    );
    assert!(
        builds::validate_toolchain("[toolchain]\ncomponents = []\n", &source.revision, "1.95.0")
            .unwrap_err()
            .to_string()
            .contains("lacks a Rust toolchain channel")
    );
}

#[test]
fn pinned_snapshot_uses_commit_blobs_instead_of_index_or_working_directory() {
    let temp = tempfile::tempdir().unwrap();
    let repo = repository(temp.path(), "source", "committed bytes\n");
    let commit = git(&repo, &["rev-parse", "HEAD"]).unwrap();
    fs::write(repo.join("tracked.txt"), "staged change\n").unwrap();
    git(&repo, &["add", "tracked.txt"]).unwrap();
    fs::write(repo.join("tracked.txt"), "unstaged change\n").unwrap();
    fs::write(repo.join("untracked.txt"), "never committed\n").unwrap();
    fs::create_dir(repo.join("target")).unwrap();
    fs::write(repo.join("target/build-output"), "large build output\n").unwrap();
    let destination = temp.path().join("snapshot");
    materialize_commit(&repo, &commit, &destination).unwrap();
    assert_eq!(
        fs::read(destination.join("tracked.txt")).unwrap(),
        b"committed bytes\n"
    );
    assert_eq!(
        fs::read(destination.join("nested/binary.dat")).unwrap(),
        [0, 255, 4, 13, 10]
    );
    assert!(!destination.join("untracked.txt").exists());
    assert!(!destination.join("target").exists());
    assert!(!destination.join(".git").exists());
    assert_eq!(
        fs::read_to_string(repo.join("tracked.txt")).unwrap(),
        "unstaged change\n"
    );
}

#[test]
fn selected_reference_head_can_come_from_an_unrelated_repository() {
    let temp = tempfile::tempdir().unwrap();
    let fork = repository(temp.path(), "fork", "fork tree\n");
    let candidate = repository(temp.path(), "candidate", "independent candidate tree\n");
    let fork_head = git(&fork, &["rev-parse", "HEAD"]).unwrap();
    let candidate_head = git(&candidate, &["rev-parse", "HEAD"]).unwrap();
    assert_ne!(fork_head, candidate_head);
    let reference = super::resolve_reference(
        &fork,
        Some(&candidate),
        temp.path().join("prepared/reference"),
    )
    .unwrap();
    assert_eq!(reference.revision, candidate_head);
    assert_eq!(
        reference.tree,
        git(&candidate, &["rev-parse", "HEAD^{tree}"]).unwrap()
    );
    assert_eq!(reference.origin, fs::canonicalize(&candidate).unwrap());
    assert_eq!(reference.selection, "HEAD");
    assert!(!reference.upstream);
    assert!(
        !reference.checkout.exists(),
        "selection must not build or create a checkout"
    );
}

#[test]
fn upstream_reference_advances_only_for_a_new_stable_release() {
    let temp = tempfile::tempdir().unwrap();
    let repo = repository(temp.path(), "fork", "upstream revision\n");
    let upstream_head = git(&repo, &["rev-parse", "HEAD"]).unwrap();
    git(&repo, &["tag", "rust-v0.9.0"]).unwrap();
    git(&repo, &["tag", "rust-v0.10.0"]).unwrap();
    git(
        &repo,
        &["update-ref", "refs/remotes/upstream/main", &upstream_head],
    )
    .unwrap();
    let reference = super::resolve_reference(&repo, None, temp.path().join("reference")).unwrap();
    assert_eq!(reference.revision, upstream_head);
    assert_eq!(reference.selection, "refs/tags/rust-v0.10.0");
    assert!(reference.upstream);
    assert_eq!(Variant::Reference.name(), "reference");
    assert_eq!(
        serde_json::to_value(Variant::Reference).unwrap(),
        json!("reference")
    );
    fs::write(repo.join("tracked.txt"), "unreleased upstream changes\n").unwrap();
    git(&repo, &["add", "tracked.txt"]).unwrap();
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "unreleased",
        ],
    )
    .unwrap();
    git(&repo, &["update-ref", "refs/remotes/upstream/main", "HEAD"]).unwrap();
    for tag in [
        "rust-v0.11.0-alpha.1",
        "rust-v9.0.0-beta.1",
        "rust-v99.0.0+build",
        "rust-vv99.0.0",
        "rust-v99.0",
        "rust-v099.0.0",
    ] {
        git(&repo, &["tag", tag]).unwrap();
    }
    let repeated = super::resolve_reference(&repo, None, temp.path().join("repeat")).unwrap();
    assert_eq!(repeated.revision, upstream_head);
    assert_eq!(repeated.selection, reference.selection);
    assert!(!repeated.checkout.exists());
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "tag",
            "-a",
            "rust-v0.11.0",
            "-m",
            "stable release",
        ],
    )
    .unwrap();
    let updated = super::resolve_reference(&repo, None, temp.path().join("updated")).unwrap();
    assert_eq!(updated.selection, "refs/tags/rust-v0.11.0");
    assert_eq!(
        updated.revision,
        git(&repo, &["rev-parse", "HEAD"]).unwrap()
    );
    assert_ne!(updated.revision, reference.revision);
    assert!(!updated.checkout.exists());
}

#[test]
fn upstream_reference_requires_a_stable_release_without_falling_back_to_main() {
    let temp = tempfile::tempdir().unwrap();
    let repo = repository(temp.path(), "fork", "unreleased\n");
    git(&repo, &["update-ref", "refs/remotes/upstream/main", "HEAD"]).unwrap();
    git(&repo, &["tag", "rust-v0.11.0-alpha.1"]).unwrap();
    let error = super::resolve_reference(&repo, None, temp.path().join("missing")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no local stable upstream release tag")
    );
    assert!(error.to_string().contains("no automatic fetch"));
    assert!(!temp.path().join("missing").exists());
}

#[test]
fn reset_restores_the_same_absolute_root_and_shared_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let prepared = temp.path().join("prepared");
    let snapshot = temp.path().join("snapshot");
    fs::create_dir(&prepared).unwrap();
    fs::create_dir_all(snapshot.join("scripts")).unwrap();
    fs::write(snapshot.join("AGENTS.md"), "identical instructions\n").unwrap();
    fs::write(snapshot.join("scripts/helper.py"), "print(7)\n").unwrap();
    let workspace = prepared.join("workspace");
    reset_workspace(
        &prepared,
        &workspace,
        &snapshot,
        &provenance::hash_tree(&snapshot).unwrap(),
    )
    .unwrap();
    let first_root = fs::canonicalize(&workspace).unwrap();
    fs::write(
        workspace.join("AGENTS.md"),
        "variant changed instructions\n",
    )
    .unwrap();
    fs::write(workspace.join("scripts/helper.py"), "print(99)\n").unwrap();
    fs::write(workspace.join("leftover.txt"), "previous attempt\n").unwrap();
    reset_workspace(
        &prepared,
        &workspace,
        &snapshot,
        &provenance::hash_tree(&snapshot).unwrap(),
    )
    .unwrap();
    assert_eq!(fs::canonicalize(&workspace).unwrap(), first_root);
    assert_eq!(
        fs::read(workspace.join("AGENTS.md")).unwrap(),
        b"identical instructions\n"
    );
    assert_eq!(
        fs::read(workspace.join("scripts/helper.py")).unwrap(),
        b"print(7)\n"
    );
    assert!(!workspace.join("leftover.txt").exists());
    assert!(workspace.join(".git").exists());
}

#[test]
fn reset_rejects_wrong_name_or_parent_without_deleting_the_target() {
    let temp = tempfile::tempdir().unwrap();
    let prepared = temp.path().join("prepared");
    let outside = temp.path().join("outside/workspace");
    let snapshot = temp.path().join("snapshot");
    let wrong_name = prepared.join("source");
    for path in [&prepared, &outside, &snapshot, &wrong_name] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(outside.join("sentinel"), "outside content").unwrap();
    fs::write(wrong_name.join("sentinel"), "source content").unwrap();
    assert!(
        reset_workspace(
            &prepared,
            &outside,
            &snapshot,
            &provenance::hash_tree(&snapshot).unwrap()
        )
        .unwrap_err()
        .to_string()
        .contains("escaped preparation")
    );
    assert!(
        reset_workspace(
            &prepared,
            &wrong_name,
            &snapshot,
            &provenance::hash_tree(&snapshot).unwrap()
        )
        .unwrap_err()
        .to_string()
        .contains("prepared workspace")
    );
    assert_eq!(
        fs::read(outside.join("sentinel")).unwrap(),
        b"outside content"
    );
    assert_eq!(
        fs::read(wrong_name.join("sentinel")).unwrap(),
        b"source content"
    );
}

#[test]
fn file_and_json_provenance_reject_changed_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("native-binary");
    fs::write(&binary, [1, 2, 3, 4]).unwrap();
    let identity = FileIdentity::record(&binary).unwrap();
    identity.verify().unwrap();
    fs::write(&binary, [1, 2, 3, 5]).unwrap();
    assert!(
        identity
            .verify()
            .unwrap_err()
            .to_string()
            .contains("changed prepared artifact")
    );
    let manifest = temp.path().join("prepared.json");
    let expected = json!({"mode":"fast","source":"recorded revision"});
    write_json(&manifest, &expected).unwrap();
    assert_eq!(read_json::<Value>(&manifest).unwrap(), expected);
    fs::write(
        &manifest,
        br#"{"mode":"full","source":"replacement revision"}"#,
    )
    .unwrap();
    assert!(
        read_json::<Value>(&manifest)
            .unwrap_err()
            .to_string()
            .contains("changed JSON artifact")
    );
}

fn runtime_feature(id: &str, key: &str, on: bool) -> Value {
    json!({"id":id,"benchmark_control":{"kind":"runtime"},"benchmark_on":on,"config_keys":[key]})
}

#[test]
fn feature_overrides_preserve_intentionally_disabled_features() {
    let features = vec![
        runtime_feature("runtime", "features.kd4_runtime", true),
        runtime_feature("active", "features.code_mode", true),
        runtime_feature("unfinished", "features.experimental_disabled", false),
        json!({"id":"instrumentation","benchmark_control":{"kind":"fixed"}}),
    ];
    assert_eq!(
        feature_overrides(&features).unwrap(),
        vec![
            "features.code_mode=true",
            "features.experimental_disabled=false",
            "features.kd4_runtime=true"
        ]
    );
}

#[test]
fn conflicting_or_unclassified_feature_controls_fail_preparation() {
    let runtime = runtime_feature("runtime", "features.kd4_runtime", true);
    let conflicting = runtime_feature("conflict", "features.kd4_runtime", false);
    assert!(
        feature_overrides(&[runtime.clone(), conflicting])
            .unwrap_err()
            .to_string()
            .contains("conflicting inventory")
    );
    assert!(
        feature_overrides(&[json!({"id":"unknown"})])
            .unwrap_err()
            .to_string()
            .contains("classification")
    );
    assert!(
        feature_overrides(&[
            runtime.clone(),
            json!({"id":"compiled","benchmark_control":{"kind":"build"}})
        ])
        .unwrap_err()
        .to_string()
        .contains("compile-time settings")
    );
    assert!(
        feature_overrides(&[
            runtime,
            runtime_feature("other", "model_reasoning_effort", true)
        ])
        .unwrap_err()
        .to_string()
        .contains("non-feature boolean control")
    );
}

#[test]
fn cargo_settings_use_the_configured_jobs_and_the_recorded_release_toolchain() {
    let environment = Environment {
        variables: BTreeMap::new(),
        tools: BTreeMap::new(),
        rust_toolchain: "pinned-toolchain-for-test".into(),
    };
    let settings = builds::settings(&environment);
    for (key, expected) in [
        ("jobs", "12"),
        ("profile", "release"),
        ("CARGO_PROFILE_RELEASE_OPT_LEVEL", "3"),
        ("CARGO_PROFILE_RELEASE_LTO", "thin"),
        ("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "4"),
        ("CARGO_PROFILE_RELEASE_INCREMENTAL", "false"),
        ("CARGO_INCREMENTAL", "0"),
        ("RUSTUP_TOOLCHAIN", "pinned-toolchain-for-test"),
    ] {
        assert_eq!(
            settings.get(key).map(String::as_str),
            Some(expected),
            "incorrect Cargo setting {key}"
        );
    }
    assert!(!settings.contains_key("CARGO_TARGET_DIR"));
}

#[test]
fn base_configuration_contains_exactly_the_approved_settings() {
    let config: toml::Value = toml::from_str(BASE_CONFIG).unwrap();
    let actual = serde_json::to_value(config).unwrap();
    let expected = json!({
        "approval_policy":"never", "sandbox_mode":"danger-full-access", "personality":"pragmatic",
        "model":"gpt-6-astra", "model_reasoning_effort":"high",
        "plan_mode_reasoning_effort":"ultra", "model_verbosity":"low", "model_reasoning_summary":"concise"
    });
    assert_eq!(
        actual, expected,
        "shared config must not inherit priority or any other settings"
    );
}

#[test]
fn project_configuration_comparison_records_explicit_differences_per_arm_without_values() {
    use super::environment::ProjectConfigComparison;
    use crate::schedule::Variant;
    use std::collections::BTreeMap;

    let temp = tempfile::tempdir().unwrap();
    let overrides = BTreeMap::from([
        (Variant::ForkOn, vec!["features.kd4_runtime=true".into()]),
        (Variant::Reference, vec![]),
    ]);
    assert!(
        ProjectConfigComparison::capture(temp.path(), BASE_CONFIG, &overrides)
            .unwrap()
            .is_none()
    );
    std::fs::create_dir(temp.path().join(".codex")).unwrap();
    let path = temp.path().join(".codex/config.toml");
    std::fs::write(&path, "approval_policy = 'never'\nmodel = 'private-model-name'\nallow_login_shell = false\n[features]\nkd4_runtime = true\n[example_settings]\nverify = 'low'\n").unwrap();
    let comparison = ProjectConfigComparison::capture(temp.path(), BASE_CONFIG, &overrides)
        .unwrap()
        .unwrap();
    let on = &comparison.by_variant[&Variant::ForkOn];
    assert_eq!(
        on.project_only,
        ["allow_login_shell", "example_settings.verify"]
    );
    assert!(on.benchmark_only.contains(&"personality".into()));
    assert_eq!(comparison.by_variant[&Variant::ForkOn].changed, ["model"]);
    assert_eq!(comparison.by_variant[&Variant::ForkOn].matching_keys, 2);
    assert_eq!(
        comparison.by_variant[&Variant::Reference].project_only,
        [
            "allow_login_shell",
            "example_settings.verify",
            "features.kd4_runtime"
        ]
    );
    let frozen = serde_json::to_string(&comparison).unwrap();
    assert!(!frozen.contains("private-model-name"));
    std::fs::write(&path, "model = 'later-change'\n").unwrap();
    assert_eq!(serde_json::to_string(&comparison).unwrap(), frozen);
    assert_ne!(
        ProjectConfigComparison::capture(temp.path(), BASE_CONFIG, &overrides)
            .unwrap()
            .unwrap()
            .sha256,
        comparison.sha256
    );
}

#[test]
fn native_shell_identity_matches_pwsh_preference_and_installed_fallbacks() {
    use super::environment::select_windows_shell;

    let modern_path = PathBuf::from(r"D:\tools\pwsh.exe");
    let legacy_path = PathBuf::from(r"D:\tools\powershell.exe");
    let modern_install = PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe");
    let legacy_install =
        PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
    // A PATH-based legacy host must not beat an installed modern host that is
    // absent from PATH. Checking only `which powershell` misses both first cases.
    for (modern_on_path, modern_installed, legacy_on_path, expected) in [
        (true, true, true, &modern_path),
        (false, true, true, &modern_install),
        (false, false, true, &legacy_path),
        (false, false, false, &legacy_install),
    ] {
        let selected = select_windows_shell(
            |name| match name {
                "pwsh" if modern_on_path => Some(modern_path.clone()),
                "powershell" if legacy_on_path => Some(legacy_path.clone()),
                _ => None,
            },
            |path| {
                (modern_installed && path == modern_install.as_path())
                    || path == legacy_install.as_path()
            },
        )
        .unwrap();
        assert_eq!(&selected, expected);
    }
}

#[test]
fn native_shell_identity_fails_when_no_supported_host_exists() {
    let failure = super::environment::select_windows_shell(|_| None, |_| false).unwrap_err();
    assert!(
        failure
            .to_string()
            .contains("no supported native PowerShell host")
    );
}

#[test]
fn reset_establishes_a_git_root_below_parent_instructions_and_config() {
    let temp = tempfile::tempdir().unwrap();
    let parent_repo = repository(temp.path(), "parent", "outer repository\n");
    fs::write(
        parent_repo.join("AGENTS.md"),
        "Unapproved parent instructions\n",
    )
    .unwrap();
    fs::create_dir(parent_repo.join(".codex")).unwrap();
    fs::write(
        parent_repo.join(".codex/config.toml"),
        "model = 'unapproved-parent-model'\n",
    )
    .unwrap();
    let prepared = parent_repo.join("target/prepared");
    let snapshot = prepared.join("snapshot");
    fs::create_dir_all(snapshot.join("nested")).unwrap();
    fs::write(snapshot.join("AGENTS.md"), "Approved task instructions\n").unwrap();
    let workspace = prepared.join("workspace");
    reset_workspace(
        &prepared,
        &workspace,
        &snapshot,
        &provenance::hash_tree(&snapshot).unwrap(),
    )
    .unwrap();
    let expected_root = fs::canonicalize(&workspace).unwrap();
    for cwd in [&workspace, &workspace.join("nested")] {
        let actual = git(cwd, &["rev-parse", "--show-toplevel"]).unwrap();
        assert_eq!(fs::canonicalize(actual).unwrap(), expected_root);
    }
    // Codex instruction and config discovery stop at this nearest .git marker;
    // a restored fixture must never resolve to the enclosing source checkout.
    assert!(workspace.join(".git").is_dir());
    assert!(!workspace.join(".codex/config.toml").exists());
    assert_eq!(
        fs::read(workspace.join("AGENTS.md")).unwrap(),
        b"Approved task instructions\n"
    );
    assert_eq!(
        fs::read(parent_repo.join("AGENTS.md")).unwrap(),
        b"Unapproved parent instructions\n"
    );
}

#[test]
fn shared_snapshot_keeps_approved_root_agents_without_nested_overrides() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("source");
    fs::create_dir_all(repo.join("scripts/nested/.codex")).unwrap();
    fs::write(repo.join("AGENTS.md"), "Approved shared instructions\n").unwrap();
    fs::write(repo.join("scripts/helper.py"), "print(7)\n").unwrap();
    fs::write(repo.join("scripts/AGENTS.md"), "Nested instructions\n").unwrap();
    fs::write(
        repo.join("scripts/nested/AGENTS.override.md"),
        "Override instructions\n",
    )
    .unwrap();
    fs::write(
        repo.join("scripts/nested/.codex/config.toml"),
        "service_tier='priority'\n",
    )
    .unwrap();
    fs::write(
        repo.join("scripts/nested/data.toml"),
        "routing='required'\n",
    )
    .unwrap();
    let snapshot = temp.path().join("shared");
    super::snapshot_shared_inputs(&repo, &snapshot).unwrap();
    assert_eq!(
        fs::read(snapshot.join("AGENTS.md")).unwrap(),
        b"Approved shared instructions\n"
    );
    assert_eq!(
        fs::read(snapshot.join("scripts/helper.py")).unwrap(),
        b"print(7)\n"
    );
    assert_eq!(
        fs::read(snapshot.join("scripts/nested/data.toml")).unwrap(),
        b"routing='required'\n"
    );
    for excluded in [
        "scripts/AGENTS.md",
        "scripts/nested/AGENTS.override.md",
        "scripts/nested/.codex/config.toml",
    ] {
        assert!(
            !snapshot.join(excluded).exists(),
            "inherited source survived: {excluded}"
        );
        assert!(
            repo.join(excluded).is_file(),
            "shared source was modified: {excluded}"
        );
    }
}

#[test]
fn reused_build_reports_lookup_time_without_replacing_original_build_time() {
    use super::provenance::hash_bytes;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("codex-rs")).unwrap();
    fs::write(
        source.join("codex-rs/Cargo.lock"),
        "pinned dependency bytes\n",
    )
    .unwrap();
    fs::write(
        source.join("codex-rs/rust-toolchain.toml"),
        "[toolchain]\nchannel = 'recorded-toolchain'\n",
    )
    .unwrap();
    let artifact = temp.path().join("native-artifact");
    fs::write(&artifact, "original native executable\n").unwrap();
    let environment = Environment {
        variables: BTreeMap::new(),
        tools: BTreeMap::new(),
        rust_toolchain: "recorded-toolchain".into(),
    };
    let settings = builds::settings(&environment);
    let requested = [("codex-app-server", "codex-app-server")];
    let lockfile = FileIdentity::record(&source.join("codex-rs/Cargo.lock")).unwrap();
    // Supply a valid prior cache record; this test exercises reuse accounting,
    // including compatibility with the original six-job cache encoding.
    let mut legacy_settings = settings;
    legacy_settings.insert("jobs".into(), "6".into());
    let key = hash_bytes(
        &serde_json::to_vec(&(
            "pinned-revision",
            &lockfile.sha256,
            Option::<&str>::None,
            &legacy_settings,
            &requested,
            &environment.tools,
        ))
        .unwrap(),
    );
    let target_root = temp.path().join("targets");
    let target_directory = target_root.join(&key);
    fs::create_dir_all(&target_directory).unwrap();
    let record = target_directory.join("repo-benchmark-build.json");
    let original = builds::BuildIdentity {
        revision: "pinned-revision".into(),
        source: source.clone(),
        target_directory: target_directory.clone(),
        build_directory: None,
        settings: legacy_settings,
        lockfile,
        cargo_config: None,
        v8_artifacts: None,
        executables: BTreeMap::from([(
            "codex-app-server".into(),
            FileIdentity::record(&artifact).unwrap(),
        )]),
        log: target_directory.join("build.log"),
        cache_key: key,
        build_elapsed_ms: 42000,
        cache_reuse_elapsed_ms: None,
    };
    write_json(&record, &original).unwrap();
    let record_bytes = fs::read(&record).unwrap();
    assert!(
        serde_json::from_slice::<serde_json::Value>(&record_bytes)
            .unwrap()
            .get("v8Artifacts")
            .is_none(),
        "legacy fork cache records must remain compatible without V8 provenance"
    );
    let started = std::time::Instant::now();
    let reused = builds::build(
        &source,
        "pinned-revision",
        &target_root,
        &environment,
        &requested,
    )
    .unwrap();
    let elapsed = started.elapsed().as_millis();
    assert_eq!(reused.build_elapsed_ms, 42000);
    assert_eq!(
        reused.cache_key, original.cache_key,
        "adding source-pinned V8 setup must not invalidate an unaffected fork cache"
    );
    assert!(reused.v8_artifacts.is_none());
    assert!(reused.build_directory.is_none());
    let lookup_ms = reused
        .cache_reuse_elapsed_ms
        .expect("cache hit must record this preparation's lookup separately");
    assert!(u128::from(lookup_ms) <= elapsed);
    assert_eq!(
        fs::read(&record).unwrap(),
        record_bytes,
        "reusing a build must preserve its original timing record"
    );
    // A new preparation has a different source path but the same released inputs.
    // No Cargo tool is provided: entering the build path would fail this test.
    let next_source = temp.path().join("next-preparation/source");
    fs::create_dir_all(next_source.join("codex-rs")).unwrap();
    for name in ["Cargo.lock", "rust-toolchain.toml"] {
        fs::copy(
            source.join("codex-rs").join(name),
            next_source.join("codex-rs").join(name),
        )
        .unwrap();
    }
    let next = builds::build(
        &next_source,
        "pinned-revision",
        &target_root,
        &environment,
        &requested,
    )
    .unwrap();
    assert_eq!(next.cache_key, original.cache_key);
    assert_eq!(
        next.executables["codex-app-server"].path,
        fs::canonicalize(&artifact).unwrap()
    );
    assert!(next.cache_reuse_elapsed_ms.is_some());
    assert_eq!(fs::read(&record).unwrap(), record_bytes);
    fs::write(
        source.join("codex-rs/rust-toolchain.toml"),
        "[toolchain]\nchannel = 'different-toolchain'\n",
    )
    .unwrap();
    let error = builds::build(
        &source,
        "pinned-revision",
        &target_root,
        &environment,
        &requested,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("native revision pinned-revision declares Rust different-toolchain"),
        "a cache hit must not bypass the shared-toolchain compatibility boundary"
    );
    assert_eq!(fs::read(&record).unwrap(), record_bytes);
}

#[test]
fn reset_reuses_unchanged_bytes_but_restores_same_size_same_time_tampering() {
    let temp = tempfile::tempdir().unwrap();
    let prepared = temp.path().join("prepared");
    let snapshot = temp.path().join("snapshot");
    fs::create_dir(&prepared).unwrap();
    fs::create_dir(&snapshot).unwrap();
    fs::write(snapshot.join("unchanged"), "keep").unwrap();
    fs::write(snapshot.join("changed"), "good").unwrap();
    fs::write(snapshot.join("deleted"), "restore").unwrap();
    let expected = provenance::hash_tree(&snapshot).unwrap();
    let workspace = prepared.join("workspace");
    reset_workspace(&prepared, &workspace, &snapshot, &expected).unwrap();
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    for name in ["unchanged", "changed"] {
        fs::File::options()
            .write(true)
            .open(workspace.join(name))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
    }
    fs::write(workspace.join("changed"), "evil").unwrap();
    fs::File::options()
        .write(true)
        .open(workspace.join("changed"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();
    fs::remove_file(workspace.join("deleted")).unwrap();
    fs::create_dir_all(workspace.join("target/cache")).unwrap();
    fs::write(workspace.join("target/cache/generated"), "large cache").unwrap();
    fs::write(workspace.join(".git/stale-attempt"), "old state").unwrap();
    reset_workspace(&prepared, &workspace, &snapshot, &expected).unwrap();
    assert_eq!(
        fs::metadata(workspace.join("unchanged"))
            .unwrap()
            .modified()
            .unwrap(),
        old,
        "unchanged content was recopied"
    );
    assert_eq!(fs::read(workspace.join("changed")).unwrap(), b"good");
    assert_eq!(fs::read(workspace.join("deleted")).unwrap(), b"restore");
    assert!(!workspace.join("target").exists());
    assert!(!workspace.join(".git/stale-attempt").exists());
    assert!(workspace.join(".git/HEAD").is_file());
}

#[test]
fn reset_rejects_snapshot_tampering_before_changing_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let prepared = temp.path().join("prepared");
    let workspace = prepared.join("workspace");
    let snapshot = temp.path().join("snapshot");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir(&snapshot).unwrap();
    fs::write(snapshot.join("source"), "approved").unwrap();
    let expected = provenance::hash_tree(&snapshot).unwrap();
    fs::write(snapshot.join("source"), "tampered").unwrap();
    fs::write(workspace.join("last-attempt"), "preserve evidence").unwrap();
    let error = reset_workspace(&prepared, &workspace, &snapshot, &expected).unwrap_err();
    assert!(error.to_string().contains("changed fixture snapshot"));
    assert_eq!(
        fs::read(workspace.join("last-attempt")).unwrap(),
        b"preserve evidence"
    );
    assert!(!workspace.join("source").exists());
}

#[test]
fn source_inventory_excludes_generated_trees_but_includes_source_changes() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/main.rs"), "first").unwrap();
    let first = provenance::source_tree_inventory(temp.path()).unwrap();
    for name in [".git", "target", "node_modules", "__pycache__"] {
        fs::create_dir_all(temp.path().join("src").join(name)).unwrap();
        fs::write(
            temp.path().join("src").join(name).join("large-cache"),
            "generated",
        )
        .unwrap();
    }
    let cached = provenance::source_tree_inventory(temp.path()).unwrap();
    assert_eq!(first.sha256, cached.sha256);
    assert_eq!(cached.files.len(), 1);
    assert_eq!(
        cached.files[&PathBuf::from("src/main.rs")],
        provenance::hash_bytes(b"first")
    );
    fs::write(temp.path().join("src/main.rs"), "other").unwrap();
    let changed = provenance::source_tree_inventory(temp.path()).unwrap();
    assert_ne!(changed.sha256, first.sha256);
    assert_eq!(
        changed.files[&PathBuf::from("src/main.rs")],
        provenance::hash_bytes(b"other")
    );
}

#[test]
fn reset_rejects_redirected_children_without_touching_external_files() {
    let temp = tempfile::tempdir().unwrap();
    let prepared = temp.path().join("prepared");
    let workspace = prepared.join("workspace");
    let snapshot = temp.path().join("snapshot");
    let outside = temp.path().join("outside");
    for path in [&workspace, &snapshot, &outside] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(outside.join("sentinel"), "outside").unwrap();
    fs::write(workspace.join("evidence"), "untouched").unwrap();
    let link = workspace.join("redirected");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let expected = provenance::hash_tree(&snapshot).unwrap();
    let error = reset_workspace(&prepared, &workspace, &snapshot, &expected).unwrap_err();
    assert!(error.to_string().contains("redirected fixture path"));
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");
    assert_eq!(fs::read(workspace.join("evidence")).unwrap(), b"untouched");
}

#[cfg(unix)]
#[test]
fn reset_restores_executable_bits_even_when_content_is_unchanged() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let prepared = temp.path().join("prepared");
    let snapshot = temp.path().join("snapshot");
    fs::create_dir(&prepared).unwrap();
    fs::create_dir(&snapshot).unwrap();
    fs::write(snapshot.join("run"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(snapshot.join("run"), fs::Permissions::from_mode(0o755)).unwrap();
    let expected = provenance::hash_tree(&snapshot).unwrap();
    let workspace = prepared.join("workspace");
    reset_workspace(&prepared, &workspace, &snapshot, &expected).unwrap();
    fs::set_permissions(workspace.join("run"), fs::Permissions::from_mode(0o644)).unwrap();
    reset_workspace(&prepared, &workspace, &snapshot, &expected).unwrap();
    assert_eq!(
        fs::metadata(workspace.join("run"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[test]
fn loaded_manifest_rejects_missing_execution_inputs_before_side_effects() {
    use super::MANIFEST_VERSION;
    use super::Prepared;
    use crate::schedule::Mode;
    use crate::schedule::Segment;
    use crate::schedule::schedule;

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let sentinel = workspace.join("preserve.txt");
    fs::write(&sentinel, "existing workspace").unwrap();
    let identity = json!({"path": sentinel, "sha256": "unused for structural validation"});
    let source = json!({"origin":workspace,"selection":"HEAD","revision":"fixture",
        "tree":"fixture","checkout":workspace,"upstream":false});
    let build = json!({"revision":"fixture","source":workspace,"targetDirectory":workspace,
        "settings":{},"lockfile":identity,"cargoConfig":null,
        "executables":{"codex-app-server":identity},"log":sentinel,"cacheKey":"fixture",
        "buildElapsedMs":0,"cacheReuseElapsedMs":null});
    let schedule = schedule(Mode::Fast);
    let fixtures: BTreeMap<_, _> = schedule
        .iter()
        .map(|attempt| {
            let name = match attempt.segment {
                Segment::Scripted => "scripted",
                Segment::RealModel => attempt.workload.as_str(),
            };
            (
                name,
                json!({"snapshot":workspace,"sha256":"fixture","descriptor":null}),
            )
        })
        .collect();
    let builds: BTreeMap<_, _> = Variant::ALL
        .into_iter()
        .map(|variant| (variant.name(), build.clone()))
        .collect();
    let overrides: BTreeMap<_, _> = Variant::ALL
        .into_iter()
        .map(|variant| (variant.name(), Vec::<String>::new()))
        .collect();
    let manifest = json!({"schemaVersion":MANIFEST_VERSION,"id":"structural-fixture",
        "directory":temp.path(),"repo":workspace,"mode":"fast","schedule":schedule,
        "workspace":workspace,"workspaceLock":temp.path().join("workspace.lock"),
        "additionalRoots":[],"runsDirectory":temp.path().join("runs"),
        "importDirectory":temp.path().join("accepted"),"fork":source,"reference":source,
        "builds":builds,"harness":identity,"harnessSources":identity,
        "environment":{"variables":{},"tools":{"python":{"executable":identity,"version":"fixture"}},"rustToolchain":"fixture"},
        "baseConfig":identity,"features":[],"featureInventory":identity,"overrides":overrides,
        "fixtures":fixtures,"sharedInputs":workspace,"sharedSha256":"fixture",
        "analyzer":identity,"analyzerFiles":[],"preparationMs":0,"budgets":{}});
    let path = temp.path().join("prepared.json");
    write_json(&path, &manifest).unwrap();
    let loaded = Prepared::load(&path).unwrap();
    assert_eq!(loaded.id, "structural-fixture");
    assert_eq!(loaded.schedule, crate::schedule::schedule(Mode::Fast));

    assert_eq!(loaded.schedule.len(), 86);
    assert_eq!(
        loaded.builds.keys().copied().collect::<Vec<_>>(),
        [Variant::ForkOn, Variant::Reference]
    );
    let error = crate::cli::run(vec![
        "compare".into(),
        "--prepared".into(),
        path.to_string_lossy().into_owned(),
    ])
    .unwrap_err();
    assert!(
        error.to_string().contains("changed prepared artifact"),
        "{error:#}"
    );
    assert!(!temp.path().join("runs").exists());

    // Reject removed variants at the normal execution boundary before touching the workspace.
    for location in ["schedule", "builds", "overrides", "projectConfigComparison"] {
        let mut invalid = manifest.clone();
        match location {
            "schedule" => invalid["schedule"][0]["variant"] = json!("fork_off"),
            "builds" => invalid["builds"]["fork_off"] = manifest["builds"]["fork_on"].clone(),
            "overrides" => invalid["overrides"]["fork_off"] = json!(["features.kd4_runtime=false"]),
            _ => {
                invalid["projectConfigComparison"] = json!({
                    "path":temp.path().join("config.toml"),"sha256":"fixture",
                    "byVariant":{"fork_off":{"changed":[],"projectOnly":[],"benchmarkOnly":[],"matchingKeys":0}}
                });
            }
        }
        write_json(&path, &invalid).unwrap();
        let error = crate::runner::execute(&path, None, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("unknown variant `fork_off`"),
            "{error:#}"
        );
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "existing workspace");
        assert!(!temp.path().join("workspace.lock").exists());
        assert!(!temp.path().join("runs").exists());
    }
    let mut legacy = manifest.clone();
    legacy["schemaVersion"] = json!(2);
    write_json(&path, &legacy).unwrap();
    let error = crate::runner::execute(&path, None, None).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unsupported prepared manifest version; prepare again")
    );
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "existing workspace");
    assert!(!temp.path().join("runs").exists());

    let mut omissions = Vec::new();
    for variant in Variant::ALL {
        let name = variant.name();
        omissions.push((
            "/overrides".to_owned(),
            name.to_owned(),
            format!("overrides for {name}"),
        ));
        omissions.push((
            "/builds".to_owned(),
            name.to_owned(),
            format!("build for {name}"),
        ));
        omissions.push((
            format!("/builds/{name}/executables"),
            "codex-app-server".into(),
            format!("codex-app-server executable for {name}"),
        ));
    }
    for fixture in fixtures.keys() {
        omissions.push((
            "/fixtures".into(),
            (*fixture).to_owned(),
            format!("fixture {fixture}"),
        ));
    }
    omissions.push((
        "/environment/tools".into(),
        "python".into(),
        "python tool".into(),
    ));
    for (mapping, key, expected) in omissions {
        let mut incomplete = manifest.clone();
        assert!(
            incomplete
                .pointer_mut(&mapping)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(&key)
                .is_some()
        );
        write_json(&path, &incomplete).unwrap();
        // execute is the normal run/compare boundary, before lock acquisition,
        // provenance checks, workspace reset, or evidence publication.
        let error = crate::runner::execute(&path, None, None).unwrap_err();
        assert!(
            format!("{error:#}").contains(&expected),
            "{mapping}/{key}: {error:#}"
        );
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "existing workspace");
        assert!(!temp.path().join("workspace.lock").exists());
        assert!(!temp.path().join("runs").exists());
        assert!(!temp.path().join("accepted").exists());
    }
}

fn lockfile(entries: &[(&str, &str, Option<&str>)]) -> String {
    let mut text = String::from("version = 4\n");
    for (name, version, source) in entries {
        text.push_str(&format!(
            "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\n"
        ));
        if let Some(source) = source {
            text.push_str(&format!(
                "source = \"{source}\"\nchecksum = \"{name}-sum\"\n"
            ));
        }
        text.push_str("dependencies = [\n \"anyhow\",\n]\n");
    }
    text
}

const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// The upstream release tag leaves its own members at their pre-release version.
#[test]
fn release_tag_lockfile_repair_accepts_only_workspace_member_versions() {
    let committed = lockfile(&[
        ("anyhow", "1.0.103", Some(REGISTRY)),
        ("codex-core", "0.0.0", None),
        ("codex-cli", "0.0.0", None),
    ]);
    let resolved = lockfile(&[
        ("anyhow", "1.0.103", Some(REGISTRY)),
        ("codex-core", "0.155.0", None),
        ("codex-cli", "0.155.0", None),
    ]);
    assert_eq!(
        super::workspace_version_only_changes(&committed, &resolved).unwrap(),
        vec!["codex-core".to_string(), "codex-cli".to_string()]
    );
    // An unchanged lockfile never reaches this repair, so a caller that got here
    // with equal inputs compared the wrong bytes.
    assert!(
        super::workspace_version_only_changes(&committed, &committed)
            .unwrap_err()
            .to_string()
            .contains("without any workspace member version difference")
    );
}

/// A repair that tolerated dependency drift would silently rebase the baseline
/// onto different third-party code, which no lockfile hash would disclose.
#[test]
fn lockfile_repair_rejects_every_difference_beyond_a_member_version() {
    let committed = lockfile(&[
        ("anyhow", "1.0.103", Some(REGISTRY)),
        ("codex-core", "0.0.0", None),
    ]);
    for (resolved, expected) in [
        (
            lockfile(&[
                ("anyhow", "1.0.104", Some(REGISTRY)),
                ("codex-core", "0.155.0", None),
            ]),
            "changed registry package anyhow",
        ),
        (
            lockfile(&[
                ("anyhow", "1.0.103", Some(REGISTRY)),
                ("codex-core", "0.155.0", None),
                ("serde", "1.0.0", Some(REGISTRY)),
            ]),
            "changed the locked package set",
        ),
        (
            lockfile(&[
                ("codex-core", "0.155.0", None),
                ("anyhow", "1.0.103", Some(REGISTRY)),
            ]),
            "reordered the locked package set",
        ),
        (
            committed.replace("version = 4", "version = 3"),
            "lockfile metadata outside the package list",
        ),
        (
            lockfile(&[
                ("anyhow", "1.0.103", Some(REGISTRY)),
                ("codex-core", "0.155.0", None),
            ])
            .replace(
                "name = \"codex-core\"\nversion = \"0.155.0\"\ndependencies = [\n \"anyhow\",\n]",
                "name = \"codex-core\"\nversion = \"0.155.0\"\ndependencies = [\n \"anyhow\",\n \"serde\",\n]",
            ),
            "beyond its workspace version",
        ),
    ] {
        let error = super::workspace_version_only_changes(&committed, &resolved).unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "expected {expected}, got {error:#}"
        );
    }
}

/// Reconciliation relaxes the clean-checkout requirement for one path only.
#[test]
fn reconciled_checkout_accepts_the_lockfile_and_nothing_else() {
    assert!(super::only_workspace_lock_modified(""));
    assert!(super::only_workspace_lock_modified(
        " M codex-rs/Cargo.lock"
    ));
    // `git` trims its output, so a lone unstaged change arrives without the
    // leading porcelain status space.
    assert!(super::only_workspace_lock_modified("M codex-rs/Cargo.lock"));
    assert!(super::only_workspace_lock_modified(
        "M  codex-rs/Cargo.lock\n M codex-rs/Cargo.lock"
    ));
    for status in [
        " M codex-rs/core/src/lib.rs",
        "M codex-rs/Cargo.lock\n M codex-rs/Cargo.toml",
        " M codex-rs/Cargo.lock.bak",
        " M Cargo.lock",
        "codex-rs/Cargo.lock",
    ] {
        assert!(
            !super::only_workspace_lock_modified(status),
            "accepted {status}"
        );
    }
}

/// A freshly resolved source has nothing to disclose until a checkout is repaired.
#[test]
fn resolved_sources_start_without_a_recorded_lockfile_repair() {
    let temp = tempfile::tempdir().unwrap();
    let origin = repository(temp.path(), "origin", "pinned");
    let source = resolve_source(&origin, "HEAD", temp.path().join("checkout"), true).unwrap();
    assert!(source.lock_reconciliation.is_none());
    assert!(
        serde_json::to_value(&source)
            .unwrap()
            .get("lockReconciliation")
            .is_none()
    );
}
