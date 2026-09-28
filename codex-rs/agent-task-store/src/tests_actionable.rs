use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn source_handoff_recaptures_scopes_without_ignored_build_churn() {
    for (scope, changed_path, accepted, legacy) in [
        ("src", "src/new.rs", false, false),
        (".", "target/output.bin", true, false),
        (".", ".git/audit-bookkeeping", true, false),
        (".", "build/tracked.rs", false, false),
        ("target/input.txt", "target/input.txt", false, false),
        (".", "src/lib.rs", false, false),
        ("src", "src/new.rs", false, true),
        ("src", "outside.txt", true, true),
        (".", "target/input.txt", false, true),
    ] {
        let fixture = Fixture::new().await;
        run_git(fixture.repo.path(), &["init", "--quiet"]);
        for directory in ["src", "build", "target"] {
            std::fs::create_dir(fixture.repo.path().join(directory)).unwrap();
        }
        for path in ["src/lib.rs", "build/tracked.rs", "target/input.txt"] {
            std::fs::write(fixture.repo.path().join(path), "original").unwrap();
        }
        std::fs::write(fixture.repo.path().join(".gitignore"), "target/\nbuild/\n").unwrap();
        run_git(fixture.repo.path(), &["add", "src/lib.rs", ".gitignore"]);
        run_git(fixture.repo.path(), &["add", "--force", "build/tracked.rs"]);
        run_git(
            fixture.repo.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
        );
        let isolated_home = TempDir::new().unwrap();
        let isolated = isolated_home.path().join("worktree");
        run_git(
            fixture.repo.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                isolated.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::create_dir(isolated.join("target")).unwrap();
        std::fs::write(isolated.join("target/input.txt"), "original").unwrap();
        let mut draft = worker_draft("scope-handoff", scope);
        draft.workspace_strategy = WorkspaceStrategy::Isolated;
        let (assignment, attempt) = fixture
            .store
            .create_assignment(&isolated, draft)
            .await
            .unwrap();
        fixture
            .store
            .submit_agent_receipt(attempt.attempt_id, completed_receipt(Vec::new()))
            .await
            .unwrap();
        if legacy {
            let revision = fixture
                .store
                .capture_workspace_revision(&isolated, vec![scope.into()])
                .await
                .unwrap();
            sqlx::query("UPDATE isolated_handoffs SET source_epoch = ?, source_manifest_hash = ?, covered_manifest_json = ? WHERE assignment_id = ?")
                .bind(revision.epoch as i64).bind(revision.manifest_hash)
                .bind(serde_json::to_string(&revision.files).unwrap()).bind(assignment.assignment_id.to_string())
                .execute(&coordination_pool(&fixture).await).await.unwrap();
        }
        let ready = fixture
            .store
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .unwrap()
            .isolation_handoff
            .unwrap();
        assert_eq!(
            ready.source_manifest_hash.starts_with("source-v1:"),
            !legacy
        );
        if scope == "." && !legacy {
            assert!(
                ready
                    .covered_manifest
                    .iter()
                    .any(|entry| entry.path == "build/tracked.rs")
            );
            assert!(
                !ready
                    .covered_manifest
                    .iter()
                    .any(|entry| entry.path.starts_with("target/")
                        || entry.path == ".git"
                        || entry.path.starts_with(".git/"))
            );
        }
        let mut integrator = worker_draft("scope-handoff", scope);
        integrator.role = AgentRole::Integrator;
        integrator.capability_profile = CapabilityProfile::IntegratorSourceWrite;
        integrator.workspace_strategy = WorkspaceStrategy::Shared;
        integrator.dependencies = vec![assignment.assignment_id];
        integrator.relation = Some(AssignmentRelation {
            kind: RelationKind::Integration,
            target_assignment_ids: vec![assignment.assignment_id],
        });
        let (_, integrator_attempt) = fixture
            .store
            .create_assignment(fixture.repo.path(), integrator)
            .await
            .unwrap();
        // Linked worktrees store .git as a file. Change harmless metadata in its gitdir.
        let changed = if changed_path.starts_with(".git/") {
            let marker = std::fs::read_to_string(isolated.join(".git")).unwrap();
            std::path::PathBuf::from(marker.trim().strip_prefix("gitdir: ").unwrap())
                .join("audit-bookkeeping")
        } else {
            isolated.join(changed_path)
        };
        if changed_path == "src/lib.rs" {
            std::fs::remove_file(&changed).unwrap();
        } else {
            std::fs::write(&changed, "changed after publication").unwrap();
        }
        let result = fixture
            .store
            .submit_agent_receipt(integrator_attempt.attempt_id, completed_receipt(Vec::new()))
            .await;
        if accepted {
            result
                .expect("ignored build and Git metadata churn must not invalidate source handoffs");
        } else {
            assert!(
                matches!(result, Err(StoreError::IsolationHandoffSuperseded(_))),
                "scope={scope}, change={changed_path}: {result:?}"
            );
        }
        let state = fixture
            .store
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .unwrap()
            .isolation_handoff
            .unwrap()
            .state;
        assert_eq!(state == IsolationHandoffState::Integrated, accepted);
    }
}

#[tokio::test]
async fn validation_manifests_preserve_unrelated_evidence_and_include_explicit_ignored_inputs() {
    for (change_during, changed_input, expect_receipt) in [
        (true, false, true),
        (true, true, false),
        (false, true, false),
    ] {
        let fixture = Fixture::new().await;
        run_git(fixture.repo.path(), &["init", "--quiet"]);
        std::fs::write(fixture.repo.path().join(".gitignore"), "input.txt\n").unwrap();
        std::fs::write(fixture.repo.path().join("input.txt"), "before").unwrap();
        std::fs::write(fixture.repo.path().join("unrelated.txt"), "before").unwrap();
        fixture
            .store
            .capture_workspace_revision(fixture.repo.path(), vec![".".into()])
            .await
            .unwrap();
        let (_, attempt) = fixture
            .store
            .create_assignment(
                fixture.repo.path(),
                validation_worker_draft("scoped-validation", ".", "focused proof"),
            )
            .await
            .unwrap();
        let call = start_focused_validation_with_evidence(
            &fixture.store,
            attempt.attempt_id,
            "input-proof",
            "focused proof",
            ValidationEvidence {
                input_paths: Some(vec!["input.txt".into()]),
                ..Default::default()
            },
        )
        .await;
        assert!(call.evidence.start_manifest_hash.is_some());
        let change = || {
            std::fs::write(
                fixture.repo.path().join(if changed_input {
                    "input.txt"
                } else {
                    "unrelated.txt"
                }),
                "after",
            )
            .unwrap()
        };
        if change_during {
            change();
            // A real capture advances workspace evidence independently of the proof inputs.
            fixture
                .store
                .capture_workspace_revision(fixture.repo.path(), vec![".".into()])
                .await
                .unwrap();
        }
        let mut terminal = call;
        terminal.evidence.validation_result = Some(serde_json::json!({
            "argv": ["focused", "proof"], "coveredPaths": ["input.txt"],
            "callId": "input-proof", "status": "succeeded", "durationMs": 1
        }));
        let terminal = finish_focused_validation(&fixture.store, terminal).await;
        if !changed_input {
            assert_ne!(
                Some(terminal.evidence.start_epoch),
                terminal.evidence.end_epoch
            );
            assert_eq!(
                terminal.evidence.start_manifest_hash,
                terminal.evidence.end_manifest_hash
            );
        }
        if !change_during {
            change();
        }
        let result = fixture
            .store
            .submit_agent_receipt(
                attempt.attempt_id,
                completed_receipt(vec!["input-proof".into()]),
            )
            .await;
        assert_eq!(
            result.is_ok(),
            expect_receipt,
            "during={change_during}, input={changed_input}: {result:?}"
        );
        if !expect_receipt {
            assert!(
                fixture
                    .store
                    .get_agent_task(attempt.assignment_id, Some(0))
                    .await
                    .unwrap()
                    .receipt
                    .is_none()
            );
        }
    }
}
