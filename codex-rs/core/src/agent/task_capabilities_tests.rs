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
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
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
            AgentRole::Explorer | AgentRole::Architect => (Vec::new(), Vec::new(), None),
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
        let mut assignment = AssignmentDraft {
            root_session_id: "root".to_string(),
            admission_origin: codex_agent_task_store::AssignmentAdmissionOrigin::Typed,
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
            contract_claims: Vec::new(),
            workspace_strategy: codex_agent_task_store::WorkspaceStrategy::Auto,
            relation,
            architecture_contract_ref: None,
        }
        .normalize(self.path())
        .expect("valid assignment fixture");
        assignment.workspace_id =
            repository_workspace_id(self.path()).expect("valid workspace fixture");
        assignment
    }
}

fn recursive_scope(path: &str) -> RepoScope {
    RepoScope {
        path: path.to_string(),
        recursive: true,
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
        "resume_agent",
        "close_agent",
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
        ("read_tool_output", TypedToolClass::ReadSearch),
        (
            codex_code_mode::PUBLIC_TOOL_NAME,
            TypedToolClass::CodeModeControl,
        ),
        (
            codex_code_mode::WAIT_TOOL_NAME,
            TypedToolClass::CodeModeControl,
        ),
        ("git_diff", TypedToolClass::Diff),
        ("shell_command", TypedToolClass::Shell),
        ("exec_command", TypedToolClass::Shell),
        ("write_stdin", TypedToolClass::Shell),
        ("apply_patch", TypedToolClass::StructuredEdit),
        ("mcp__server__read", TypedToolClass::DynamicExternal),
        ("future_unclassified_tool", TypedToolClass::Unknown),
    ] {
        assert_eq!(classify_typed_tool(None, name, None), class);
    }
}

#[test]
fn namespaces_cannot_spoof_core_or_collaboration_tools() {
    let collaboration_namespace = Some("agents");
    assert_eq!(
        classify_typed_tool(Some("agents"), "send_message", collaboration_namespace),
        TypedToolClass::AgentCommunication
    );
    assert_eq!(
        classify_typed_tool(Some("agents"), "spawn_agent", collaboration_namespace),
        TypedToolClass::RootTaskControl
    );
    assert_eq!(
        classify_typed_tool(Some("agents"), "apply_patch", collaboration_namespace),
        TypedToolClass::Unknown
    );
    assert_eq!(
        classify_typed_tool(Some("foreign"), "apply_patch", collaboration_namespace,),
        TypedToolClass::DynamicExternal
    );
    assert_eq!(
        classify_typed_tool(Some(""), "read_tool_output", None),
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
fn typed_agents_inherit_every_non_root_tool_class() {
    for class in [
        TypedToolClass::AgentCommunication,
        TypedToolClass::OwnTask,
        TypedToolClass::ReadSearch,
        TypedToolClass::CodeModeControl,
        TypedToolClass::Diff,
        TypedToolClass::Shell,
        TypedToolClass::StructuredEdit,
        TypedToolClass::DynamicExternal,
        TypedToolClass::Unknown,
    ] {
        assert!(authorize_typed_tool(class).is_ok());
    }
    assert_eq!(
        authorize_typed_tool(TypedToolClass::RootTaskControl),
        Err(CapabilityPolicyError::RootTaskControlDenied)
    );
}

#[test]
fn independent_review_sources_include_automatic_and_builtin_reviewers() {
    assert!(is_independent_review_source(&SessionSource::SubAgent(
        SubAgentSource::Review
    )));
    assert!(is_independent_review_source(&SessionSource::SubAgent(
        SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("reviewer".to_string()),
        }
    )));
    assert!(!is_independent_review_source(&SessionSource::SubAgent(
        SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("verifier".to_string()),
        }
    )));
}

#[test]
fn independent_review_shell_policy_is_read_only_on_every_shell_route() {
    let reviewer = SessionSource::SubAgent(SubAgentSource::Review);
    assert!(validate_independent_review_shell(&reviewer, true, false, false).is_ok());
    let err = validate_independent_review_shell(&reviewer, false, false, false)
        .expect_err("reviewers must reject commands that are not proven read-only");
    assert!(err.contains("Get-Command"));
    assert!(err.contains("without assignments or script blocks"));
    assert!(validate_independent_review_shell(&reviewer, true, true, false).is_err());
    assert!(validate_independent_review_shell(&reviewer, true, false, true).is_err());
    assert!(validate_independent_review_stdin(&reviewer, "").is_ok());
    assert!(validate_independent_review_stdin(&reviewer, "y\n").is_err());

    assert!(validate_independent_review_shell(&SessionSource::Cli, false, true, true).is_ok());
    assert!(validate_independent_review_stdin(&SessionSource::Cli, "y\n").is_ok());
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
        start_epoch: 0,
        end_epoch: Some(1),
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
