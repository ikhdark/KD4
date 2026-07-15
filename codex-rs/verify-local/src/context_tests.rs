use super::*;
use crate::model::PlanMode;
use crate::model::PlanRequest;
use crate::model::RawPath;
use crate::model::RepositorySnapshot;
use crate::model::Verdict;
use crate::planner::plan_verification;

#[test]
fn surface_rules_match_exact_prefix_and_glob() {
    let rule = SurfaceRule {
        id: "rules".to_string(),
        paths: vec![
            "exact.txt".to_string(),
            "src".to_string(),
            "docs/*.md".to_string(),
        ],
        owned_packages: Vec::new(),
        test_expr: None,
        validation_command: None,
        regen_command: None,
        skip_owner_tests: false,
        hash_paths: Vec::new(),
    };
    assert!(rule.matches("exact.txt"));
    assert!(rule.matches("src/nested.rs"));
    assert!(rule.matches("docs/readme.md"));
    assert!(!rule.matches("other.txt"));
}

#[test]
fn reverse_closure_is_transitive_and_cycle_safe() {
    let graph = CargoGraph {
        reverse_dependencies: BTreeMap::from([
            ("a".to_string(), BTreeSet::from(["b".to_string()])),
            ("b".to_string(), BTreeSet::from(["c".to_string()])),
            ("c".to_string(), BTreeSet::from(["a".to_string()])),
        ]),
        ..CargoGraph::default()
    };
    assert_eq!(
        graph.reverse_closure(&["a".to_string()]),
        BTreeSet::from(["a".to_string(), "b".to_string(), "c".to_string()])
    );
}

#[test]
fn fast_owner_source_scope_uses_compile_check_and_test_scope_uses_tests() {
    let repository = fixture_repository();
    let source = plan_for(repository.path(), PlanMode::Fast, "codex-rs/a/src/lib.rs");
    assert!(
        source
            .commands
            .iter()
            .any(|command| command.id == "owner-check:a")
    );
    let test = plan_for(
        repository.path(),
        PlanMode::Fast,
        "codex-rs/a/tests/route.rs",
    );
    assert!(
        test.commands
            .iter()
            .any(|command| command.id == "owner-test:a")
    );
}

#[test]
fn multiple_unowned_dirty_areas_need_scope() {
    let repository = fixture_repository();
    let mut snapshot = RepositorySnapshot::from_explicit_paths(
        repository.path(),
        [
            RawPath::from_utf8("docs/a.md"),
            RawPath::from_utf8("sdk/a.py"),
        ],
    )
    .expect("snapshot");
    snapshot.source = crate::model::SnapshotSource::Worktree;
    let plan = plan_verification(
        PlanRequest {
            mode: Some(PlanMode::Plan),
            ..PlanRequest::default()
        },
        snapshot,
    );
    assert_eq!(plan.verdict, Some(Verdict::NeedsScope));
    assert_eq!(
        plan.scope.as_ref().map(|scope| scope.dirty_groups.len()),
        Some(2)
    );
}

fn plan_for(repository: &Path, mode: PlanMode, path: &str) -> crate::model::PlanEnvelopeV2 {
    let raw = RawPath::from_utf8(path);
    let snapshot =
        RepositorySnapshot::from_explicit_paths(repository, [raw.clone()]).expect("snapshot");
    plan_verification(
        PlanRequest {
            mode: Some(mode),
            changed: vec![raw],
            ..PlanRequest::default()
        },
        snapshot,
    )
}

fn fixture_repository() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    write(
        temp.path().join("codex-rs/Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\n",
    );
    write(
        temp.path().join("codex-rs/a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
    );
    write(temp.path().join("codex-rs/a/src/lib.rs"), "pub fn a() {}\n");
    write(
        temp.path().join("codex-rs/b/Cargo.toml"),
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\n[dependencies]\na = { path = \"../a\" }\n",
    );
    write(temp.path().join("codex-rs/b/src/lib.rs"), "pub fn b() {}\n");
    write(
        temp.path().join("scripts/verify_local_rules.toml"),
        "# empty fixture rules\n",
    );
    temp
}

fn write(path: PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write fixture");
}
