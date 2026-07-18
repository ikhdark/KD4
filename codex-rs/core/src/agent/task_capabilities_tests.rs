use super::*;
use codex_agent_task_store::AcceptanceCriterion;
use codex_agent_task_store::AgentRole;
use codex_agent_task_store::AssignmentDraft;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::AssignmentRelation;
use codex_agent_task_store::AttemptId;
use codex_agent_task_store::AttributionConfidence;
use codex_agent_task_store::MutationEvidence;
use codex_agent_task_store::RelationKind;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;

struct RepoFixture {
    root: TempDir,
}

impl RepoFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary repository");
        for directory in [
            "src/nested",
            "codex-rs/protocol/src",
            "codex-rs/target/debug",
            ".codex/verify-local",
            "build/artifacts",
        ] {
            std::fs::create_dir_all(root.path().join(directory))
                .expect("repository fixture directory");
        }
        Self { root }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn assignment(&self, profile: CapabilityProfile, write_scope: Vec<RepoScope>) -> Assignment {
        let role = match profile {
            CapabilityProfile::ReadSearch => AgentRole::Explorer,
            CapabilityProfile::ReadSearchDiff => AgentRole::Reviewer,
            CapabilityProfile::ReadSearchShell => AgentRole::Verifier,
            CapabilityProfile::ScopedSourceWrite => AgentRole::Worker,
            CapabilityProfile::IntegratorSourceWrite => AgentRole::Integrator,
        };
        let target = AssignmentId::new();
        let (write_scope, dependencies, relation) = match role {
            AgentRole::Explorer => (Vec::new(), Vec::new(), None),
            AgentRole::Worker => (write_scope, Vec::new(), None),
            AgentRole::Reviewer => (
                Vec::new(),
                vec![target],
                Some(AssignmentRelation {
                    kind: RelationKind::Review,
                    target_assignment_ids: vec![target],
                }),
            ),
            AgentRole::Verifier => (
                Vec::new(),
                vec![target],
                Some(AssignmentRelation {
                    kind: RelationKind::Verification,
                    target_assignment_ids: vec![target],
                }),
            ),
            AgentRole::Integrator => (
                write_scope,
                vec![target],
                Some(AssignmentRelation {
                    kind: RelationKind::Integration,
                    target_assignment_ids: vec![target],
                }),
            ),
        };
        AssignmentDraft {
            root_session_id: "root".to_string(),
            role,
            capability_profile: profile,
            objective: "exercise pure capability policy".to_string(),
            acceptance_criteria: vec![AcceptanceCriterion {
                id: "criterion".to_string(),
                text: "policy is deterministic".to_string(),
            }],
            read_scope: Vec::new(),
            write_scope,
            stop_condition: "stop after policy evaluation".to_string(),
            dependencies,
            risk_hints: Vec::new(),
            required_evidence: Vec::new(),
            prohibited_changes: Vec::new(),
            relation,
        }
        .normalize(self.path())
        .expect("valid assignment fixture")
    }
}

fn request(class: TypedToolClass) -> TypedToolRequest<'static> {
    TypedToolRequest {
        class,
        external_mutation_intent: ExternalMutationIntent::ProvenReadOnly,
        repo_paths: &[],
    }
}

fn recursive_scope(path: &str) -> RepoScope {
    RepoScope {
        path: path.to_string(),
        recursive: true,
    }
}

fn exact_scope(path: &str) -> RepoScope {
    RepoScope {
        path: path.to_string(),
        recursive: false,
    }
}

#[test]
fn tool_classification_separates_typed_authority() {
    for name in ["send_message", "wait_agent", "list_agents"] {
        assert_eq!(
            classify_typed_tool(None, name, None),
            TypedToolClass::AgentCommunication
        );
    }
    for name in ["get_agent_task", "submit_agent_receipt"] {
        assert_eq!(
            classify_typed_tool(None, name, None),
            TypedToolClass::OwnTask
        );
    }
    for name in [
        "spawn_agent",
        "send_input",
        "followup_task",
        "interrupt_agent",
        "amend_agent_task",
        "waive_agent_gate",
        "abandon_agent_task",
    ] {
        assert_eq!(
            classify_typed_tool(None, name, None),
            TypedToolClass::RootTaskControl
        );
    }
    for (name, class) in [
        ("search_source", TypedToolClass::ReadSearch),
        ("read_file_span", TypedToolClass::ReadSearch),
        ("git_diff", TypedToolClass::Diff),
        ("shell_command", TypedToolClass::Shell),
        ("apply_patch", TypedToolClass::StructuredEdit),
        ("mcp__server__read", TypedToolClass::DynamicExternal),
        ("future_unclassified_tool", TypedToolClass::Unknown),
    ] {
        assert_eq!(classify_typed_tool(None, name, None), class);
    }
}

#[test]
fn namespaces_cannot_spoof_core_or_collaboration_tools() {
    let collaboration_namespace = Some("collaboration");
    assert_eq!(
        classify_typed_tool(
            Some("collaboration"),
            "send_message",
            collaboration_namespace
        ),
        TypedToolClass::AgentCommunication
    );
    assert_eq!(
        classify_typed_tool(
            Some("collaboration"),
            "spawn_agent",
            collaboration_namespace
        ),
        TypedToolClass::RootTaskControl
    );
    assert_eq!(
        classify_typed_tool(
            Some("collaboration"),
            "apply_patch",
            collaboration_namespace
        ),
        TypedToolClass::Unknown
    );
    assert_eq!(
        classify_typed_tool(Some("foreign"), "apply_patch", collaboration_namespace),
        TypedToolClass::DynamicExternal
    );
    assert_eq!(
        classify_typed_tool(Some(""), "search_source", None),
        TypedToolClass::DynamicExternal
    );
    assert_eq!(
        classify_typed_tool(None, "send_message", collaboration_namespace),
        TypedToolClass::Unknown
    );
    assert_eq!(
        classify_typed_tool(None, "Apply_Patch", None),
        TypedToolClass::Unknown
    );
}

#[test]
fn profiles_receive_only_their_declared_tool_classes() {
    let fixture = RepoFixture::new();
    let profiles = [
        CapabilityProfile::ReadSearch,
        CapabilityProfile::ReadSearchDiff,
        CapabilityProfile::ReadSearchShell,
        CapabilityProfile::ScopedSourceWrite,
        CapabilityProfile::IntegratorSourceWrite,
    ];
    for profile in profiles {
        let assignment = fixture.assignment(profile, vec![recursive_scope("src")]);
        for class in [
            TypedToolClass::AgentCommunication,
            TypedToolClass::OwnTask,
            TypedToolClass::ReadSearch,
        ] {
            assert!(authorize_typed_tool(&assignment, fixture.path(), request(class)).is_ok());
        }
        assert_eq!(
            authorize_typed_tool(
                &assignment,
                fixture.path(),
                request(TypedToolClass::RootTaskControl)
            ),
            Err(CapabilityPolicyError::RootTaskControlDenied)
        );
        assert_eq!(
            authorize_typed_tool(&assignment, fixture.path(), request(TypedToolClass::Diff))
                .is_ok(),
            matches!(
                profile,
                CapabilityProfile::ReadSearchDiff
                    | CapabilityProfile::ScopedSourceWrite
                    | CapabilityProfile::IntegratorSourceWrite
            )
        );
        assert_eq!(
            authorize_typed_tool(&assignment, fixture.path(), request(TypedToolClass::Shell))
                .is_ok(),
            matches!(
                profile,
                CapabilityProfile::ReadSearchShell
                    | CapabilityProfile::ScopedSourceWrite
                    | CapabilityProfile::IntegratorSourceWrite
            )
        );
        assert_eq!(
            authorize_typed_tool(
                &assignment,
                fixture.path(),
                request(TypedToolClass::StructuredEdit)
            ),
            if matches!(
                profile,
                CapabilityProfile::ScopedSourceWrite | CapabilityProfile::IntegratorSourceWrite
            ) {
                Err(CapabilityPolicyError::MissingStructuredEditPaths)
            } else {
                Err(CapabilityPolicyError::ToolDenied {
                    profile,
                    class: TypedToolClass::StructuredEdit,
                })
            }
        );
        assert_eq!(
            authorize_typed_tool(
                &assignment,
                fixture.path(),
                request(TypedToolClass::Unknown)
            ),
            Err(CapabilityPolicyError::UnknownToolDenied)
        );
    }
}

#[test]
fn root_controls_and_external_mutations_fail_closed_for_every_profile() {
    let fixture = RepoFixture::new();
    for profile in [
        CapabilityProfile::ReadSearch,
        CapabilityProfile::ReadSearchDiff,
        CapabilityProfile::ReadSearchShell,
        CapabilityProfile::ScopedSourceWrite,
        CapabilityProfile::IntegratorSourceWrite,
    ] {
        let assignment = fixture.assignment(profile, vec![recursive_scope("src")]);
        assert!(
            authorize_typed_tool(
                &assignment,
                fixture.path(),
                request(TypedToolClass::DynamicExternal)
            )
            .is_ok()
        );
        assert_eq!(
            authorize_typed_tool(
                &assignment,
                fixture.path(),
                TypedToolRequest {
                    class: TypedToolClass::DynamicExternal,
                    external_mutation_intent: ExternalMutationIntent::MayMutate,
                    repo_paths: &[],
                },
            ),
            Err(CapabilityPolicyError::ExternalMutationDenied)
        );
    }
}

#[test]
fn forged_role_profile_pairs_fail_closed() {
    let fixture = RepoFixture::new();
    let mut assignment = fixture.assignment(CapabilityProfile::ReadSearch, Vec::new());
    assignment.capability_profile = CapabilityProfile::ScopedSourceWrite;
    assert!(matches!(
        authorize_typed_tool(
            &assignment,
            fixture.path(),
            request(TypedToolClass::ReadSearch)
        ),
        Err(CapabilityPolicyError::RoleProfileMismatch {
            role: AgentRole::Explorer,
            expected: CapabilityProfile::ReadSearch,
            actual: CapabilityProfile::ScopedSourceWrite,
        })
    ));
}

#[test]
fn structured_edits_require_complete_canonical_scope_coverage() {
    let fixture = RepoFixture::new();
    let paths = vec![
        "src\\nested\\mod.rs".to_string(),
        "Cargo.toml".to_string(),
        "src/lib.rs".to_string(),
        "src/lib.rs".to_string(),
    ];
    for profile in [
        CapabilityProfile::ScopedSourceWrite,
        CapabilityProfile::IntegratorSourceWrite,
    ] {
        let assignment = fixture.assignment(
            profile,
            vec![exact_scope("Cargo.toml"), recursive_scope("src")],
        );
        assert_eq!(
            authorize_typed_tool(
                &assignment,
                fixture.path(),
                TypedToolRequest {
                    class: TypedToolClass::StructuredEdit,
                    external_mutation_intent: ExternalMutationIntent::MayMutate,
                    repo_paths: &paths,
                },
            )
            .expect("scoped structured edit")
            .normalized_repo_paths,
            vec![
                "Cargo.toml".to_string(),
                "src/lib.rs".to_string(),
                "src/nested/mod.rs".to_string(),
            ]
        );
    }

    let worker = fixture.assignment(
        CapabilityProfile::ScopedSourceWrite,
        vec![exact_scope("Cargo.toml"), recursive_scope("src")],
    );
    for outside in ["src2/lib.rs", "Cargo.toml/child"] {
        let paths = vec![outside.to_string()];
        assert_eq!(
            authorize_typed_tool(
                &worker,
                fixture.path(),
                TypedToolRequest {
                    class: TypedToolClass::StructuredEdit,
                    external_mutation_intent: ExternalMutationIntent::MayMutate,
                    repo_paths: &paths,
                },
            ),
            Err(CapabilityPolicyError::PathOutsideWriteScope(
                outside.to_string()
            ))
        );
    }
}

#[test]
fn invalid_repository_paths_are_rejected_before_authorization() {
    let fixture = RepoFixture::new();
    let worker = fixture.assignment(
        CapabilityProfile::ScopedSourceWrite,
        vec![recursive_scope("src")],
    );
    for invalid in [
        "",
        "   ",
        "/tmp/file",
        "../file",
        "src/../file",
        "src/./file",
        "src//file",
        "C:\\outside\\file",
        "\\\\server\\share\\file",
        "src/\0file",
    ] {
        let paths = vec![invalid.to_string()];
        assert!(matches!(
            authorize_typed_tool(
                &worker,
                fixture.path(),
                TypedToolRequest {
                    class: TypedToolClass::StructuredEdit,
                    external_mutation_intent: ExternalMutationIntent::MayMutate,
                    repo_paths: &paths,
                },
            ),
            Err(CapabilityPolicyError::InvalidRepoPath { .. })
        ));
    }
}

#[cfg(windows)]
#[test]
fn windows_scope_matching_is_separator_and_case_insensitive() {
    let fixture = RepoFixture::new();
    let worker = fixture.assignment(
        CapabilityProfile::ScopedSourceWrite,
        vec![recursive_scope("src")],
    );
    let paths = vec!["SRC\\LIB.RS".to_string(), "src/lib.rs".to_string()];
    assert_eq!(
        authorize_typed_tool(
            &worker,
            fixture.path(),
            TypedToolRequest {
                class: TypedToolClass::StructuredEdit,
                external_mutation_intent: ExternalMutationIntent::MayMutate,
                repo_paths: &paths,
            },
        )
        .expect("Windows path aliases are in scope")
        .normalized_repo_paths,
        vec!["SRC/LIB.RS".to_string()]
    );
}

#[test]
fn assignments_are_bound_to_their_canonical_repository() {
    let first = RepoFixture::new();
    let second = RepoFixture::new();
    let assignment = first.assignment(CapabilityProfile::ReadSearch, Vec::new());
    assert!(
        authorize_typed_tool(
            &assignment,
            &first.path().join("."),
            request(TypedToolClass::ReadSearch)
        )
        .is_ok()
    );
    assert!(matches!(
        authorize_typed_tool(
            &assignment,
            second.path(),
            request(TypedToolClass::ReadSearch)
        ),
        Err(CapabilityPolicyError::RepositoryMismatch { .. })
    ));

    let missing_root = first.path().join("missing-repository");
    assert!(matches!(
        authorize_typed_tool(
            &assignment,
            &missing_root,
            request(TypedToolClass::ReadSearch)
        ),
        Err(CapabilityPolicyError::InvalidRepositoryRoot { .. })
    ));
}

#[test]
fn symlink_and_dangling_symlink_escapes_fail_closed() {
    let fixture = RepoFixture::new();
    let worker = fixture.assignment(
        CapabilityProfile::ScopedSourceWrite,
        vec![recursive_scope("src")],
    );
    let outside = tempfile::tempdir().expect("outside directory");
    if create_dir_symlink(outside.path(), &fixture.path().join("src/escape")).is_err() {
        // Creating symlinks may require an opt-in privilege on Windows.
        return;
    }
    let escaped = vec!["src/escape/new.rs".to_string()];
    assert!(matches!(
        authorize_typed_tool(
            &worker,
            fixture.path(),
            TypedToolRequest {
                class: TypedToolClass::StructuredEdit,
                external_mutation_intent: ExternalMutationIntent::MayMutate,
                repo_paths: &escaped,
            },
        ),
        Err(CapabilityPolicyError::InvalidRepoPath { .. })
    ));

    let dangling_target = outside.path().join("not-created.rs");
    if create_file_symlink(&dangling_target, &fixture.path().join("src/dangling.rs")).is_err() {
        return;
    }
    let dangling = vec!["src/dangling.rs".to_string()];
    assert!(matches!(
        authorize_typed_tool(
            &worker,
            fixture.path(),
            TypedToolRequest {
                class: TypedToolClass::StructuredEdit,
                external_mutation_intent: ExternalMutationIntent::MayMutate,
                repo_paths: &dangling,
            },
        ),
        Err(CapabilityPolicyError::InvalidRepoPath { .. })
    ));
}

#[cfg(unix)]
fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[test]
fn verifier_writable_roots_are_profile_repository_and_scope_bound() {
    let fixture = RepoFixture::new();
    let verifier = fixture.assignment(CapabilityProfile::ReadSearchShell, Vec::new());
    let roots = vec![
        recursive_scope("codex-rs/target"),
        recursive_scope(".codex/verify-local"),
        recursive_scope("build"),
    ];
    for allowed in [
        "codex-rs/target",
        "codex-rs/target/debug/x",
        ".codex\\verify-local\\log",
        "build/artifacts/result.json",
    ] {
        assert!(verifier_can_write_path(&verifier, fixture.path(), &roots, allowed).unwrap());
    }
    for denied in ["codex-rs/targeted/x", "builder/result", "src/lib.rs"] {
        assert!(!verifier_can_write_path(&verifier, fixture.path(), &roots, denied).unwrap());
    }

    let worker = fixture.assignment(
        CapabilityProfile::ScopedSourceWrite,
        vec![recursive_scope("src")],
    );
    assert!(
        !verifier_can_write_path(&worker, fixture.path(), &roots, "codex-rs/target/debug/x")
            .unwrap()
    );
    assert!(typed_agent_can_write_path(&worker, fixture.path(), &roots, "src/lib.rs").unwrap());
    assert!(
        !typed_agent_can_write_path(
            &worker,
            fixture.path(),
            &roots,
            "build/artifacts/result.json"
        )
        .unwrap()
    );
    let reviewer = fixture.assignment(CapabilityProfile::ReadSearchDiff, Vec::new());
    assert!(!typed_agent_can_write_path(&reviewer, fixture.path(), &roots, "src/lib.rs").unwrap());
    for invalid in ["../target", "/tmp/target", "C:\\target"] {
        assert!(matches!(
            verifier_can_write_path(&verifier, fixture.path(), &roots, invalid),
            Err(CapabilityPolicyError::InvalidRepoPath { .. })
        ));
    }
}

#[test]
fn cold_review_context_is_attempt_bound_and_structurally_excludes_worker_history() {
    let fixture = RepoFixture::new();
    let assignment = fixture.assignment(
        CapabilityProfile::ScopedSourceWrite,
        vec![recursive_scope("src")],
    );
    let attempt_id = AttemptId::new();
    let evidence = MutationEvidence {
        assignment_id: assignment.assignment_id,
        attempt_id,
        path: "src/lib.rs".to_string(),
        pre_write_hash: Some("before".to_string()),
        pre_write_existed: true,
        final_hash: Some("after".to_string()),
        final_write_existed: Some(true),
        mutation_event_ids: Vec::new(),
        attribution_confidence: AttributionConfidence::Definitive,
        snapshot_retained: true,
        first_observed_at: chrono::Utc::now(),
        finalized_at: Some(chrono::Utc::now()),
    };
    let context = build_cold_review_context(
        fixture.path(),
        ColdReviewContextInput {
            assignment: assignment.clone(),
            attempt_id,
            applicable_instructions: vec!["nearest AGENTS policy".to_string()],
            attempt_specific_diff: "diff --git a/src/lib.rs b/src/lib.rs".to_string(),
            observed_writes: vec![evidence.clone()],
            relevant_contracts: vec!["source owner contract".to_string()],
            nearest_tests: vec!["owner_test".to_string()],
        },
    )
    .expect("valid cold-review context");
    let serialized = serde_json::to_value(&context).expect("serialize cold-review context");
    let keys = serialized
        .as_object()
        .expect("cold-review context object")
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        BTreeSet::from([
            "applicable_instructions".to_string(),
            "assignment".to_string(),
            "attempt_id".to_string(),
            "attempt_specific_diff".to_string(),
            "nearest_tests".to_string(),
            "observed_writes".to_string(),
            "relevant_contracts".to_string(),
        ])
    );
    let encoded = serialized.to_string();
    assert!(!encoded.contains("worker_reasoning"));
    assert!(!encoded.contains("conversation_history"));

    let mut wrong_attempt = evidence.clone();
    wrong_attempt.attempt_id = AttemptId::new();
    assert!(matches!(
        build_cold_review_context(
            fixture.path(),
            ColdReviewContextInput {
                assignment: assignment.clone(),
                attempt_id,
                applicable_instructions: Vec::new(),
                attempt_specific_diff: String::new(),
                observed_writes: vec![wrong_attempt],
                relevant_contracts: Vec::new(),
                nearest_tests: Vec::new(),
            }
        ),
        Err(CapabilityPolicyError::ColdReviewAttemptMismatch { .. })
    ));

    let mut wrong_assignment = evidence;
    wrong_assignment.assignment_id = AssignmentId::new();
    assert!(matches!(
        build_cold_review_context(
            fixture.path(),
            ColdReviewContextInput {
                assignment,
                attempt_id,
                applicable_instructions: Vec::new(),
                attempt_specific_diff: String::new(),
                observed_writes: vec![wrong_assignment],
                relevant_contracts: Vec::new(),
                nearest_tests: Vec::new(),
            }
        ),
        Err(CapabilityPolicyError::ColdReviewAssignmentMismatch { .. })
    ));
}

fn base_risk_input<'a>() -> RiskPolicyInput<'a> {
    RiskPolicyInput {
        changed_paths: &[],
        configured_high_risk_paths: &[],
        touched_contracts: &[],
        configured_high_risk_contracts: &[],
        cross_owner_scope: false,
        named_domains: &[],
        non_generated_changed_files: 0,
        non_generated_changed_lines: 0,
        focused_validation_succeeded: true,
        ownership_conflict: false,
        drift: false,
    }
}

#[test]
fn risk_thresholds_and_closed_reasons_are_exact_and_deterministic() {
    let fixture = RepoFixture::new();
    let worker = fixture.assignment(
        CapabilityProfile::ScopedSourceWrite,
        vec![recursive_scope("src")],
    );
    let at_limits = derive_risk_policy(
        &worker,
        fixture.path(),
        RiskPolicyInput {
            non_generated_changed_files: 5,
            non_generated_changed_lines: 400,
            ..base_risk_input()
        },
    )
    .unwrap();
    assert!(!at_limits.decision.review_required);

    let all_domains = [
        RiskDomain::Concurrency,
        RiskDomain::UnsafeCode,
        RiskDomain::Lifecycle,
        RiskDomain::Persistence,
        RiskDomain::Schema,
        RiskDomain::Protocol,
        RiskDomain::Security,
        RiskDomain::Installation,
    ];
    let over_limits = derive_risk_policy(
        &worker,
        fixture.path(),
        RiskPolicyInput {
            cross_owner_scope: true,
            named_domains: &all_domains,
            non_generated_changed_files: 6,
            non_generated_changed_lines: 401,
            focused_validation_succeeded: false,
            ownership_conflict: true,
            drift: true,
            ..base_risk_input()
        },
    )
    .unwrap();
    assert_eq!(
        over_limits.decision.reasons,
        vec![
            "cross-owner scope",
            "concurrency risk",
            "unsafe risk",
            "lifecycle risk",
            "persistence risk",
            "schema risk",
            "protocol risk",
            "security risk",
            "installation risk",
            "more than five non-generated changed files",
            "more than 400 non-generated changed lines",
            "missing successful focused validation",
            "ownership conflict",
            "concurrent drift",
        ]
    );
}

#[test]
fn configured_paths_and_normalized_contracts_drive_review() {
    let fixture = RepoFixture::new();
    let worker = fixture.assignment(
        CapabilityProfile::ScopedSourceWrite,
        vec![recursive_scope("codex-rs")],
    );
    let changed_paths = vec!["codex-rs\\protocol\\src\\lib.rs".to_string()];
    let touched_contracts = vec!["  STORED__Session  ".to_string()];
    let configured_contracts = vec!["stored session".to_string()];
    let derived = derive_risk_policy(
        &worker,
        fixture.path(),
        RiskPolicyInput {
            changed_paths: &changed_paths,
            configured_high_risk_paths: &[recursive_scope("codex-rs/protocol")],
            touched_contracts: &touched_contracts,
            configured_high_risk_contracts: &configured_contracts,
            ..base_risk_input()
        },
    )
    .unwrap();
    assert!(derived.matched_high_risk_path);
    assert!(derived.matched_high_risk_contract);
    assert!(derived.facts.configured_high_risk_path);
    assert_eq!(
        derived.decision.reasons,
        vec!["configured high-risk contract or path"]
    );
}

#[test]
fn integrator_and_invalid_risk_evidence_fail_closed() {
    let fixture = RepoFixture::new();
    let integrator = fixture.assignment(
        CapabilityProfile::IntegratorSourceWrite,
        vec![recursive_scope("codex-rs")],
    );
    let cross_owner = derive_risk_policy(&integrator, fixture.path(), base_risk_input()).unwrap();
    assert!(cross_owner.facts.cross_owner_scope);
    assert_eq!(cross_owner.decision.reasons, vec!["cross-owner scope"]);

    let invalid_changed_path = vec!["../outside".to_string()];
    assert!(matches!(
        derive_risk_policy(
            &integrator,
            fixture.path(),
            RiskPolicyInput {
                changed_paths: &invalid_changed_path,
                ..base_risk_input()
            }
        ),
        Err(CapabilityPolicyError::InvalidRepoPath { .. })
    ));

    let changed_path = vec!["codex-rs/lib.rs".to_string()];
    assert!(matches!(
        derive_risk_policy(
            &integrator,
            fixture.path(),
            RiskPolicyInput {
                changed_paths: &changed_path,
                configured_high_risk_paths: &[recursive_scope("../outside")],
                ..base_risk_input()
            }
        ),
        Err(CapabilityPolicyError::InvalidRepoPath { .. })
    ));
}
