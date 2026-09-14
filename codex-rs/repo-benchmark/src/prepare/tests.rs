use super::builds;
use super::environment::{BASE_CONFIG, Environment};
use super::feature_overrides;
use super::provenance::{
    FileIdentity, git, materialize_commit, read_json, reset_workspace, write_json,
};
use super::resolve_source;
use crate::schedule::Variant;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

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
fn native_worktree_uses_the_exact_prepared_absolute_destination() {
    let temp = tempfile::tempdir().unwrap();
    let repo = repository(temp.path(), "source", "pinned native contents\n");
    let snapshot = PathBuf::from("nested").join(format!("{}.snap", "long-snapshot-name".repeat(8)));
    fs::write(repo.join(&snapshot), "pinned snapshot bytes\n").unwrap();
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
    super::checkout(&source).unwrap();
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
    let reference = resolve_source(
        &candidate,
        "HEAD",
        temp.path().join("prepared/reference"),
        false,
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
fn upstream_source_metadata_keeps_stable_reference_variant_name() {
    let temp = tempfile::tempdir().unwrap();
    let repo = repository(temp.path(), "fork", "upstream revision\n");
    let upstream_head = git(&repo, &["rev-parse", "HEAD"]).unwrap();
    git(
        &repo,
        &["update-ref", "refs/remotes/upstream/main", &upstream_head],
    )
    .unwrap();
    let reference =
        resolve_source(&repo, "upstream/main", temp.path().join("reference"), true).unwrap();
    assert_eq!(reference.revision, upstream_head);
    assert_eq!(reference.selection, "upstream/main");
    assert!(reference.upstream);
    assert_eq!(Variant::Reference.name(), "reference");
    assert_eq!(
        serde_json::to_value(Variant::Reference).unwrap(),
        json!("reference")
    );
    let error = resolve_source(
        &repo,
        "refs/remotes/missing/main",
        temp.path().join("missing"),
        true,
    )
    .unwrap_err();
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
    reset_workspace(&prepared, &workspace, &snapshot).unwrap();
    let first_root = fs::canonicalize(&workspace).unwrap();
    fs::write(
        workspace.join("AGENTS.md"),
        "variant changed instructions\n",
    )
    .unwrap();
    fs::write(workspace.join("scripts/helper.py"), "print(99)\n").unwrap();
    fs::write(workspace.join("leftover.txt"), "previous attempt\n").unwrap();
    reset_workspace(&prepared, &workspace, &snapshot).unwrap();
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
        reset_workspace(&prepared, &outside, &snapshot)
            .unwrap_err()
            .to_string()
            .contains("escaped preparation")
    );
    assert!(
        reset_workspace(&prepared, &wrong_name, &snapshot)
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
        feature_overrides(&features, true).unwrap(),
        vec![
            "features.code_mode=true",
            "features.experimental_disabled=false",
            "features.kd4_runtime=true"
        ]
    );
    assert_eq!(
        feature_overrides(&features, false).unwrap(),
        vec![
            "features.code_mode=false",
            "features.experimental_disabled=false",
            "features.kd4_runtime=false"
        ]
    );
}

#[test]
fn conflicting_or_unclassified_feature_controls_fail_preparation() {
    let runtime = runtime_feature("runtime", "features.kd4_runtime", true);
    let conflicting = runtime_feature("conflict", "features.kd4_runtime", false);
    assert!(
        feature_overrides(&[runtime.clone(), conflicting], true)
            .unwrap_err()
            .to_string()
            .contains("conflicting inventory")
    );
    assert!(
        feature_overrides(&[json!({"id":"unknown"})], false)
            .unwrap_err()
            .to_string()
            .contains("classification")
    );
    assert!(
        feature_overrides(
            &[
                runtime.clone(),
                json!({"id":"compiled","benchmark_control":{"kind":"build"}})
            ],
            true
        )
        .unwrap_err()
        .to_string()
        .contains("compile-time ablation")
    );
    assert!(
        feature_overrides(
            &[
                runtime,
                runtime_feature("other", "model_reasoning_effort", true)
            ],
            true
        )
        .unwrap_err()
        .to_string()
        .contains("non-feature boolean control")
    );
}

#[test]
fn cargo_settings_use_six_jobs_and_the_recorded_release_toolchain() {
    let environment = Environment {
        variables: BTreeMap::new(),
        tools: BTreeMap::new(),
        rust_toolchain: "pinned-toolchain-for-test".into(),
    };
    let settings = builds::settings(&environment);
    for (key, expected) in [
        ("jobs", "6"),
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
        "model":"gpt-6-astra", "model_reasoning_effort":"high", "approvals_reviewer":"user",
        "plan_mode_reasoning_effort":"ultra", "model_verbosity":"low", "model_reasoning_summary":"concise",
        "model_auto_compact_token_limit":129000, "model_auto_compact_token_limit_scope":"total"
    });
    assert_eq!(
        actual, expected,
        "shared config must not inherit priority or any other settings"
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
    reset_workspace(&prepared, &workspace, &snapshot).unwrap();
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
    // not the cache-key algorithm or a Cargo build.
    let key = hash_bytes(
        &serde_json::to_vec(&(
            "pinned-revision",
            &lockfile.sha256,
            Option::<&str>::None,
            &settings,
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
        settings,
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
    let lookup_ms = reused
        .cache_reuse_elapsed_ms
        .expect("cache hit must record this preparation's lookup separately");
    assert!(u128::from(lookup_ms) <= elapsed);
    assert_eq!(
        fs::read(&record).unwrap(),
        record_bytes,
        "reusing a build must preserve its original timing record"
    );
}
