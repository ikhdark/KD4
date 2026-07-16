use super::*;

#[test]
fn new_workspace_member_matched_by_glob_invalidates_fingerprint() {
    let fixture = fixture_repository(true);
    let request = request(fixture.path());
    let before = complete_fingerprint(&request, &build_inventory(&request, &BTreeSet::new()));
    write(
        fixture.path().join("codex-rs/crates/b/Cargo.toml"),
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        fixture.path().join("codex-rs/crates/b/src/lib.rs"),
        "pub fn b() {}\n",
    );
    let after = complete_fingerprint(&request, &build_inventory(&request, &BTreeSet::new()));
    assert_ne!(before, after);
}

#[test]
fn every_optional_toolchain_and_config_marker_invalidates_when_created() {
    let fixture = fixture_repository(true);
    let request = request(fixture.path());
    let baseline = complete_fingerprint(&request, &build_inventory(&request, &BTreeSet::new()));
    for relative in [
        "rust-toolchain",
        "rust-toolchain.toml",
        ".cargo/config",
        ".cargo/config.toml",
        "codex-rs/rust-toolchain",
        "codex-rs/rust-toolchain.toml",
        "codex-rs/.cargo/config",
        "codex-rs/.cargo/config.toml",
    ] {
        let path = fixture.path().join(relative);
        write(path.clone(), "# marker\n");
        let changed = complete_fingerprint(&request, &build_inventory(&request, &BTreeSet::new()));
        assert_ne!(baseline, changed, "{relative}");
        std::fs::remove_file(path).expect("remove marker");
    }
}

#[test]
fn automatic_targets_and_external_path_dependencies_invalidate() {
    let fixture = fixture_repository(true);
    let manifest = fixture.path().join("codex-rs/crates/a/Cargo.toml");
    write(
        manifest,
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\next = { path = \"../../../external/ext\" }\n",
    );
    write(
        fixture.path().join("external/ext/Cargo.toml"),
        "[package]\nname = \"ext\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        fixture.path().join("external/ext/src/lib.rs"),
        "pub fn ext() {}\n",
    );
    let request = request(fixture.path());
    let before = complete_fingerprint(&request, &build_inventory(&request, &BTreeSet::new()));
    write(
        fixture.path().join("codex-rs/crates/a/src/bin/new.rs"),
        "fn main() {}\n",
    );
    let target_changed =
        complete_fingerprint(&request, &build_inventory(&request, &BTreeSet::new()));
    assert_ne!(before, target_changed);
    write(
        fixture.path().join("external/ext/examples/demo.rs"),
        "fn main() {}\n",
    );
    let external_changed =
        complete_fingerprint(&request, &build_inventory(&request, &BTreeSet::new()));
    assert_ne!(target_changed, external_changed);
}

#[test]
fn invocation_arguments_and_environment_mode_are_fingerprint_inputs() {
    let fixture = fixture_repository(true);
    let base = request(fixture.path());
    let inventory = build_inventory(&base, &BTreeSet::new());
    let baseline = complete_fingerprint(&base, &inventory);
    let mut featured = base.clone();
    featured.extra_args = vec![OsString::from("--features"), OsString::from("feature-a")];
    assert_ne!(
        baseline,
        complete_fingerprint(&featured, &build_inventory(&featured, &BTreeSet::new()))
    );
    let mut isolated = base.clone();
    isolated.environment_mode = "isolated".to_string();
    assert_ne!(
        baseline,
        complete_fingerprint(&isolated, &build_inventory(&isolated, &BTreeSet::new()))
    );
}

#[test]
fn malformed_workspace_specs_and_globs_bypass_warm_caching() {
    let fixture = fixture_repository(true);
    write(
        fixture.path().join("codex-rs/Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\", 7]\nexclude = [\"[\"]\nresolver = \"2\"\n",
    );
    let inventory = build_inventory(&request(fixture.path()), &BTreeSet::new());
    assert!(!inventory.complete);
    assert!(
        inventory
            .reasons
            .iter()
            .any(|reason| reason.contains("workspace.members[1]"))
    );
    assert!(
        inventory
            .reasons
            .iter()
            .any(|reason| reason.contains("invalid workspace exclude glob"))
    );
}

#[test]
fn optional_discovery_inputs_must_have_the_expected_file_type() {
    let fixture = fixture_repository(true);
    std::fs::create_dir_all(fixture.path().join("codex-rs/.cargo/config.toml"))
        .expect("config directory");
    let inventory = build_inventory(&request(fixture.path()), &BTreeSet::new());
    assert!(!inventory.complete);
    assert!(
        inventory
            .reasons
            .iter()
            .any(|reason| reason.contains("wrong file type"))
    );
}

#[test]
fn workspace_inherited_paths_and_explicit_targets_are_fingerprint_inputs() {
    let fixture = fixture_repository(true);
    write(
        fixture.path().join("codex-rs/Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n[workspace.dependencies]\next = { path = \"../external/ext\" }\n",
    );
    write(
        fixture.path().join("codex-rs/crates/a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\next = { workspace = true }\n[[bin]]\nname = \"tool\"\npath = \"../../../tools/tool.rs\"\n",
    );
    write(
        fixture.path().join("external/ext/Cargo.toml"),
        "[package]\nname = \"ext\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        fixture.path().join("external/ext/src/lib.rs"),
        "pub fn ext() {}\n",
    );
    write(fixture.path().join("tools/tool.rs"), "fn main() {}\n");
    let request = request(fixture.path());
    let before = complete_fingerprint(&request, &build_inventory(&request, &BTreeSet::new()));
    write(
        fixture.path().join("external/ext/src/lib.rs"),
        "pub fn ext() { println!(\"changed\"); }\n",
    );
    let dependency_changed =
        complete_fingerprint(&request, &build_inventory(&request, &BTreeSet::new()));
    assert_ne!(before, dependency_changed);
    write(
        fixture.path().join("tools/tool.rs"),
        "fn main() { println!(\"changed\"); }\n",
    );
    let target_changed =
        complete_fingerprint(&request, &build_inventory(&request, &BTreeSet::new()));
    assert_ne!(dependency_changed, target_changed);
}

#[test]
fn unmodeled_metadata_topology_arguments_bypass_warm_caching() {
    let fixture = fixture_repository(true);
    let mut request = request(fixture.path());
    request.extra_args = vec![
        OsString::from("--manifest-path"),
        OsString::from("../other/Cargo.toml"),
    ];
    let inventory = build_inventory(&request, &BTreeSet::new());
    assert!(!inventory.complete);
    assert!(
        inventory
            .reasons
            .iter()
            .any(|reason| reason.contains("not cache-enumerable"))
    );
}

#[test]
fn metadata_manifest_inventory_and_environment_inputs_fail_closed() {
    assert!(metadata_manifest_paths(&serde_json::json!({"packages": [{}]})).is_err());
    for key in [
        "CARGO_HOME",
        "RUSTC",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTUP_HOME",
        "RUSTDOC",
        "PATH",
    ] {
        assert!(is_relevant_environment_key(OsStr::new(key)), "{key}");
    }
    assert_eq!(
        resolve_cargo_home(
            Path::new("C:/workspace"),
            Some(OsString::from("relative-cargo-home")),
            None,
        ),
        Some(PathBuf::from("C:/workspace").join("relative-cargo-home"))
    );
}

#[test]
fn locked_complete_metadata_gets_a_real_warm_hit() {
    let fixture = fixture_repository(true);
    let request = request(fixture.path());
    let first = load_cargo_metadata(&request).expect("cold metadata");
    assert_eq!(first.disposition, CargoCacheDisposition::MissStored);
    let second = load_cargo_metadata(&request).expect("warm metadata");
    assert_eq!(second.disposition, CargoCacheDisposition::Hit);
    assert_eq!(first.fingerprint, second.fingerprint);
    assert_eq!(first.metadata, second.metadata);
}

#[test]
fn missing_lockfile_never_creates_a_warm_entry_for_that_run() {
    let fixture = fixture_repository(false);
    let request = request(fixture.path());
    let result = load_cargo_metadata(&request).expect("fresh unlocked metadata");
    assert!(matches!(
        result.disposition,
        CargoCacheDisposition::Bypassed { .. }
    ));
    let index = fixture
        .path()
        .join(".codex/verify-local/cargo-graph-v1/index");
    assert!(
        !index.exists()
            || std::fs::read_dir(index)
                .expect("index dir")
                .next()
                .is_none()
    );
}

#[cfg(unix)]
#[test]
fn symlinked_inputs_bypass_warm_caching() {
    use std::os::unix::fs::symlink;
    let fixture = fixture_repository(true);
    let rules = fixture.path().join("scripts/verify_local_rules.toml");
    std::fs::remove_file(&rules).expect("remove rules");
    symlink(fixture.path().join("codex-rs/Cargo.toml"), &rules).expect("symlink");
    let inventory = build_inventory(&request(fixture.path()), &BTreeSet::new());
    assert!(!inventory.complete);
    assert!(
        inventory
            .reasons
            .iter()
            .any(|reason| reason.contains("symlinked"))
    );
}

fn request(repository: &Path) -> CargoMetadataRequest {
    let mut request = CargoMetadataRequest::for_repository(repository);
    request.repository_root = std::fs::canonicalize(repository).expect("canonical repo");
    request.workspace_root =
        std::fs::canonicalize(repository.join("codex-rs")).expect("canonical workspace");
    request
}

fn fixture_repository(with_lock: bool) -> tempfile::TempDir {
    let fixture = tempfile::tempdir().expect("tempdir");
    write(
        fixture.path().join("codex-rs/Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
    );
    write(
        fixture.path().join("codex-rs/crates/a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        fixture.path().join("codex-rs/crates/a/src/lib.rs"),
        "pub fn a() {}\n",
    );
    write(
        fixture.path().join("scripts/verify_local_rules.toml"),
        "# fixture rules\n",
    );
    if with_lock {
        write(
            fixture.path().join("codex-rs/Cargo.lock"),
            "# This file is automatically @generated by Cargo.\nversion = 4\n\n[[package]]\nname = \"a\"\nversion = \"0.1.0\"\n",
        );
    }
    git(fixture.path(), &["init", "-q"]);
    fixture
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?}");
}

fn write(path: PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write fixture");
}
