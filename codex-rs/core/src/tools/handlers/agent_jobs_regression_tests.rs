use super::*;
use pretty_assertions::assert_eq;
use sqlx::Connection as _;

#[test]
fn explicit_empty_records_keep_cardinality_and_width() {
    let (_, rows) = parse_csv("value\n\n\"\"\nfirst\n\"\"\n").unwrap();
    assert_eq!(rows, vec![vec![""], vec!["first"], vec![""]]);
    let (_, rows) = parse_csv("a,b\n1,first\n,\n,,\n2,last\n").unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[1], vec!["", ""]);
    assert_eq!(rows[2], vec!["", "", ""]);
    assert!(parse_csv("").unwrap().0.is_empty());
    assert!(parse_csv("header\n").unwrap().1.is_empty());
}

#[test]
fn item_ids_preserve_suffix_order_with_linear_duplicate_probes() {
    fn baseline(bases: &[&str]) -> Vec<String> {
        let mut seen = HashSet::new();
        bases
            .iter()
            .map(|base| {
                let mut id = base.to_string();
                let mut suffix = 2;
                while !seen.insert(id.clone()) {
                    id = format!("{base}-{suffix}");
                    suffix += 1;
                }
                id
            })
            .collect()
    }
    let alphabet = ["x", "x-2", "x-3", "x-2-2", "row-1", "é", " "];
    for length in 0..=5_u32 {
        for mut code in 0..alphabet.len().pow(length) {
            let bases = (0..length)
                .map(|_| {
                    let base = alphabet[code % alphabet.len()];
                    code /= alphabet.len();
                    base
                })
                .collect::<Vec<_>>();
            let mut ids = JobItemIds::default();
            let actual = bases
                .iter()
                .map(|id| ids.allocate(id.to_string()).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(actual, baseline(&bases));
        }
    }
    let mut duplicates = JobItemIds::default();
    for index in 1..=4_000 {
        assert_eq!(
            duplicates.allocate("x".into()).unwrap(),
            if index == 1 {
                "x".into()
            } else {
                format!("x-{index}")
            }
        );
    }
    assert_eq!(duplicates.probes, 7_999);
    let mut unique = JobItemIds::default();
    for index in 0..4_000 {
        unique.allocate(format!("unique-{index}")).unwrap();
    }
    assert_eq!(unique.probes, 4_000);
    assert!(unique.next_suffix.is_empty());
}

#[test]
fn default_outputs_keep_full_identity_and_bound_the_stem() {
    let dir = tempfile::tempdir().unwrap();
    let input = AbsolutePathBuf::from_absolute_path(dir.path().join("input.csv")).unwrap();
    let first = "12345678-0000-4000-8000-000000000001";
    let second = "12345678-0000-4000-8000-000000000002";
    let a = default_output_csv_path(&input, first);
    let b = default_output_csv_path(&input, second);
    assert_ne!(a, b);
    assert!(a.display().to_string().contains(first));
    let long =
        AbsolutePathBuf::from_absolute_path(dir.path().join(format!("{}.csv", "é".repeat(120))))
            .unwrap();
    let output = default_output_csv_path(&long, first);
    assert!(
        output
            .as_path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .len()
            < 240
    );
}

#[tokio::test]
async fn empty_wrong_width_and_nested_invocations_fail_before_admission() -> anyhow::Result<()> {
    for mode in [MultiAgentVersion::V1, MultiAgentVersion::V2] {
        let mut fixture = csv_job_sql_fault_fixture(mode).await?;
        let turn = Arc::get_mut(&mut fixture.turn).unwrap();
        turn.session_source =
            SessionSource::SubAgent(SubAgentSource::Other("agent_job:outer".into()));
        let error = spawn_agents_on_csv::handle(
            fixture.session.clone(),
            crate::session::step_context::StepContext::for_test(fixture.turn.clone()),
            fixture.arguments.clone(),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("nested CSV jobs are unsupported"), "{error}");
        Arc::get_mut(&mut fixture.turn).unwrap().session_source = SessionSource::Cli;
        let args: Value = serde_json::from_str(&fixture.arguments)?;
        tokio::fs::write(args["csv_path"].as_str().unwrap(), "a,b\n1,ok\n,,\n").await?;
        let error = csv_job_fault_handler_error(&fixture).await;
        assert!(error.contains("3 fields but header has 2"), "{error}");
        let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_jobs")
            .fetch_one(&mut fixture.connection)
            .await?;
        assert_eq!(jobs, 0);
        assert!(!tokio::fs::try_exists(&fixture.output_path).await?);
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn actual_v2_permit_release_wakes_bulk_waiter() {
    let control = crate::agent::AgentControl::default();
    let permit = control
        .reserve_execution_capacity(
            MultiAgentVersion::V2,
            &SessionSource::SubAgent(SubAgentSource::Other("unrelated".into())),
        )
        .unwrap();
    let mut capacity = control.subscribe_capacity_changes();
    let (_jobs, mut jobs) = tokio::sync::watch::channel(0);
    let active = HashMap::new();
    let cancellation = CancellationToken::new();
    let start = Instant::now();
    let wait = wait_for_status_change(
        &active,
        &mut jobs,
        &mut capacity,
        &cancellation,
        Duration::from_secs(60),
    );
    tokio::pin!(wait);
    assert!(futures::poll!(&mut wait).is_pending());
    drop(permit);
    wait.await;
    assert_eq!(Instant::now(), start);
}

#[tokio::test]
async fn stale_worker_finalization_cannot_fail_a_deleted_workers_replacement() -> anyhow::Result<()>
{
    let (_dir, db, job) = create_running_job(1).await;
    let (mut session, turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        turn.config.model_provider.clone(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    Arc::get_mut(&mut session).unwrap().services.agent_control = manager.agent_control();
    let old = ThreadId::new();
    assert!(
        db.mark_agent_job_item_running_with_thread(&job.id, "item-0", &old.to_string())
            .await?
    );
    db.delete_thread(old).await?;
    let replacement = manager.start_thread((*turn.config).clone()).await?;
    assert!(
        db.mark_agent_job_item_running_with_thread(
            &job.id,
            "item-0",
            &replacement.thread_id.to_string()
        )
        .await?
    );
    finalize_finished_item(session.clone(), db.clone(), &job.id, "item-0", old).await?;
    let item = db.get_agent_job_item(&job.id, "item-0").await?.unwrap();
    assert_eq!(item.status, codex_state::AgentJobItemStatus::Running);
    assert_eq!(
        item.assigned_thread_id,
        Some(replacement.thread_id.to_string())
    );
    assert!(
        db.report_agent_job_item_result(
            &job.id,
            "item-0",
            &replacement.thread_id.to_string(),
            &json!({"kept":true})
        )
        .await?
    );
    shutdown_agent_job_worker(&session, replacement.thread_id).await?;
    Ok(())
}

#[tokio::test]
async fn recovery_publication_retries_a_real_rename_failure() -> anyhow::Result<()> {
    let (dir, db, job) = create_running_job(1).await;
    db.mark_agent_job_failed(&job.id, "stopped").await?;
    std::fs::create_dir(&job.output_csv_path)?;
    assert!(
        reconcile_orphaned_agent_jobs_after_restart(db.clone())
            .await
            .is_err()
    );
    std::fs::remove_dir(&job.output_csv_path)?;
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(codex_state::state_db_path(&dir.path().join("state"))),
    )
    .await?;
    sqlx::query("UPDATE agent_job_recovery_exports SET retry_at = 0")
        .execute(&mut connection)
        .await?;
    reconcile_orphaned_agent_jobs_after_restart(db.clone()).await?;
    let csv = tokio::fs::read_to_string(&job.output_csv_path).await?;
    assert!(csv.contains("item-0"));
    assert!(db.claim_agent_job_recovery_exports().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_contended_publisher_cannot_install_a_stale_snapshot() -> anyhow::Result<()> {
    let (_dir, db, job) = create_running_job(1).await;
    let worker = ThreadId::new().to_string();
    assert!(
        db.mark_agent_job_item_running_with_thread(&job.id, "item-0", &worker)
            .await?
    );
    let lock = codex_file_system::try_acquire_atomic_write_lock(std::path::Path::new(
        &job.output_csv_path,
    ))?
    .unwrap();
    assert!(export_job_csv_snapshot(db.clone(), &job).await.is_err());
    assert!(
        db.report_agent_job_item_result(&job.id, "item-0", &worker, &json!({"revision":2}))
            .await?
    );
    drop(lock);
    // Even a caller retaining the original job object selects rows only after
    // publication ownership; it cannot replay an old pre-lock row snapshot.
    export_job_csv_snapshot(db.clone(), &job).await?;
    let csv = tokio::fs::read_to_string(&job.output_csv_path).await?;
    assert!(csv.contains("completed") && csv.contains("revision"));
    Ok(())
}

#[tokio::test]
async fn an_explicit_empty_record_reaches_the_real_ledger_and_export() -> anyhow::Result<()> {
    let fixture = csv_job_sql_fault_fixture(MultiAgentVersion::V2).await?;
    let args: Value = serde_json::from_str(&fixture.arguments)?;
    tokio::fs::write(args["csv_path"].as_str().unwrap(), "value\n\"\"\n").await?;
    let output = spawn_agents_on_csv::handle(
        fixture.session.clone(),
        crate::session::step_context::StepContext::for_test(fixture.turn.clone()),
        fixture.arguments.clone(),
        CancellationToken::new(),
    )
    .await?;
    let receipt: Value = serde_json::from_str(&output.into_text())?;
    assert_eq!(receipt["total_items"], 1);
    assert_eq!(
        receipt["failed_items"], 1,
        "unavailable worker startup is an explicit row failure, never zero-row success"
    );
    let items = fixture
        .db
        .list_agent_job_items(receipt["job_id"].as_str().unwrap(), None, None)
        .await?;
    assert_eq!(items[0].row_index, 0);
    assert_eq!(items[0].row_json, json!({"value":""}));
    let csv = tokio::fs::read_to_string(&fixture.output_path).await?;
    assert_eq!(parse_csv(&csv).unwrap().1.len(), 1);
    fixture.connection.close().await?;
    Ok(())
}

#[tokio::test]
async fn bulk_binding_registers_accounting_and_row_cleanup_drains_descendants() -> anyhow::Result<()>
{
    let (_dir, db, job) = create_running_job(1).await;
    let (mut session, turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        turn.config.model_provider.clone(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let control = manager.agent_control();
    let mut config = (*turn.config).clone();
    config.agent_max_depth = 4;
    let parent = manager.start_thread(config.clone()).await?;
    let unrelated = manager.start_thread(config.clone()).await?;
    Arc::get_mut(&mut session).unwrap().services.agent_control = control.clone();
    let input = || {
        vec![UserInput::Text {
            text: "Process the assigned row".into(),
            text_elements: vec![],
        }]
    };
    let worker = control
        .spawn_agent_with_metadata(
            config.clone(),
            input(),
            Some(SessionSource::SubAgent(SubAgentSource::Other(format!(
                "agent_job:{}",
                job.id
            )))),
            SpawnAgentOptions {
                parent_thread_id: Some(parent.thread_id),
                agent_job_binding: Some(AgentJobBinding {
                    state_db: db.clone(),
                    job_id: job.id.clone(),
                    item_id: "item-0".into(),
                }),
                ..Default::default()
            },
        )
        .await?;
    let usage = control
        .aggregate_live_subtree_usage(parent.thread_id)
        .await?;
    assert_eq!(
        usage.descendant_threads, 1,
        "fresh membership cannot depend on backfill"
    );
    assert!(!usage.membership_complete);
    let mut ancestor = worker.thread_id;
    let mut children = Vec::new();
    for depth in 1..=2 {
        let child = control
            .spawn_agent_with_metadata(
                config.clone(),
                input(),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: ancestor,
                    depth,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                })),
                SpawnAgentOptions {
                    parent_thread_id: Some(ancestor),
                    ..Default::default()
                },
            )
            .await?;
        ancestor = child.thread_id;
        children.push(ancestor);
    }
    assert!(
        db.report_agent_job_item_result(
            &job.id,
            "item-0",
            &worker.thread_id.to_string(),
            &json!({"kept":true})
        )
        .await?
    );
    finalize_finished_item(session, db.clone(), &job.id, "item-0", worker.thread_id).await?;
    for thread in std::iter::once(worker.thread_id).chain(children) {
        assert!(manager.get_thread(thread).await.is_err());
    }
    assert!(manager.get_thread(unrelated.thread_id).await.is_ok());
    assert_eq!(
        db.get_agent_job_item(&job.id, "item-0")
            .await?
            .unwrap()
            .result_json,
        Some(json!({"kept":true}))
    );
    assert_eq!(
        db.list_agent_job_attempts(&job.id, "item-0").await?,
        vec![(1, worker.thread_id.to_string())]
    );
    control.shutdown_live_agent(parent.thread_id).await?;
    control.shutdown_live_agent(unrelated.thread_id).await?;
    Ok(())
}

#[tokio::test]
async fn existing_worker_deadline_is_serviced_while_another_spawn_is_paused() -> anyhow::Result<()>
{
    let (dir, db, job) = create_running_job(2).await;
    let (mut session, turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        turn.config.model_provider.clone(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let control = manager.agent_control();
    Arc::get_mut(&mut session).unwrap().services.agent_control = control.clone();
    let parent = manager.start_thread((*turn.config).clone()).await?;
    Arc::get_mut(&mut session).unwrap().thread_id = parent.thread_id;
    let worker = manager.start_thread((*turn.config).clone()).await?;
    assert!(
        db.mark_agent_job_item_running_with_thread(
            &job.id,
            "item-0",
            &worker.thread_id.to_string()
        )
        .await?
    );
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(codex_state::state_db_path(&dir.path().join("state"))),
    )
    .await?;
    sqlx::query("UPDATE agent_jobs SET max_runtime_seconds = 1 WHERE id = ?")
        .bind(&job.id)
        .execute(&mut connection)
        .await?;
    let barrier = Arc::new(crate::agent::control::AgentControlTestBarrier::default());
    control.set_before_initial_submission_barrier(Some(barrier.clone()));
    let cancellation = CancellationToken::new();
    let runner = tokio::spawn({
        let db = db.clone();
        let session = session.clone();
        let job_id = job.id.clone();
        let cancellation = cancellation.clone();
        async move {
            let options = JobRunnerOptions {
                max_concurrency: 2,
                spawn_config: (*turn.config).clone(),
                environments: turn.environments.clone(),
            };
            run_agent_job_loop(
                session,
                turn,
                db,
                job_id,
                options,
                cancellation,
                &mut HashMap::new(),
            )
            .await
        }
    });
    timeout(Duration::from_secs(5), barrier.wait_until_reached()).await?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    timeout(Duration::from_secs(5), async {
        while !manager.captured_ops().iter().any(|(id, op)| {
            *id == worker.thread_id && matches!(op, codex_protocol::protocol::Op::Shutdown)
        }) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    // No barrier release is needed: cancellation drops the suspended spawn and
    // its existing rollback owner drains the hidden child.
    cancellation.cancel();
    timeout(Duration::from_secs(15), runner).await???;
    assert_eq!(db.get_agent_job_progress(&job.id).await?.running_items, 0);
    assert_eq!(manager.list_thread_ids().await, vec![parent.thread_id]);
    control.shutdown_live_agent(parent.thread_id).await?;
    Ok(())
}

#[tokio::test]
async fn released_spawn_progresses_while_an_existing_worker_is_serviced() -> anyhow::Result<()> {
    let (dir, db, job) = create_running_job(2).await;
    let (mut session, turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        turn.config.model_provider.clone(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let control = manager.agent_control();
    Arc::get_mut(&mut session).unwrap().services.agent_control = control.clone();
    let parent = manager.start_thread((*turn.config).clone()).await?;
    Arc::get_mut(&mut session).unwrap().thread_id = parent.thread_id;
    let worker = manager.start_thread((*turn.config).clone()).await?;
    assert!(
        db.mark_agent_job_item_running_with_thread(
            &job.id,
            "item-0",
            &worker.thread_id.to_string()
        )
        .await?
    );
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(codex_state::state_db_path(&dir.path().join("state"))),
    )
    .await?;
    sqlx::query("UPDATE agent_jobs SET max_runtime_seconds = 1 WHERE id = ?")
        .bind(&job.id)
        .execute(&mut connection)
        .await?;
    // Servicing the stale worker cannot finish until this is released. It
    // stands in for servicing that waits on a state-DB write lock held by a
    // transaction inside the other, in-flight spawn.
    let release_worker_shutdown =
        crate::test_support::block_thread_terminal_tasks(worker.thread.as_ref());
    let barrier = Arc::new(crate::agent::control::AgentControlTestBarrier::default());
    control.set_before_initial_submission_barrier(Some(barrier.clone()));
    let cancellation = CancellationToken::new();
    let runner = tokio::spawn({
        let db = db.clone();
        let session = session.clone();
        let job_id = job.id.clone();
        let cancellation = cancellation.clone();
        async move {
            let options = JobRunnerOptions {
                max_concurrency: 2,
                spawn_config: (*turn.config).clone(),
                environments: turn.environments.clone(),
            };
            run_agent_job_loop(
                session,
                turn,
                db,
                job_id,
                options,
                cancellation,
                &mut HashMap::new(),
            )
            .await
        }
    });
    timeout(Duration::from_secs(5), barrier.wait_until_reached()).await?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    timeout(Duration::from_secs(5), async {
        while !manager.captured_ops().iter().any(|(id, op)| {
            *id == worker.thread_id && matches!(op, codex_protocol::protocol::Op::Shutdown)
        }) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    // The runner is now blocked servicing the stale worker. The other spawn
    // must still reach its initial submission once released.
    barrier.release_one();
    let submitted = timeout(Duration::from_secs(5), async {
        while !manager.captured_ops().iter().any(|(id, op)| {
            *id != parent.thread_id
                && *id != worker.thread_id
                && matches!(op, codex_protocol::protocol::Op::UserInput { .. })
        }) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    drop(release_worker_shutdown);
    submitted.map_err(|_| {
        anyhow::anyhow!("the released spawn waited for existing-worker servicing to finish")
    })?;
    cancellation.cancel();
    timeout(Duration::from_secs(15), runner).await???;
    assert_eq!(db.get_agent_job_progress(&job.id).await?.running_items, 0);
    assert_eq!(manager.list_thread_ids().await, vec![parent.thread_id]);
    control.shutdown_live_agent(parent.thread_id).await?;
    Ok(())
}

#[tokio::test]
async fn revoked_runner_stops_only_its_retained_workers_and_preserves_failure() -> anyhow::Result<()>
{
    let (dir, db, job) = create_running_job(1).await;
    let (mut session, turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        turn.config.model_provider.clone(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    Arc::get_mut(&mut session).unwrap().services.agent_control = manager.agent_control();
    let worker = manager.start_thread((*turn.config).clone()).await?;
    assert!(
        db.mark_agent_job_item_running_with_thread(
            &job.id,
            "item-0",
            &worker.thread_id.to_string()
        )
        .await?
    );
    let observer =
        codex_state::StateRuntime::init(dir.path().join("state"), "test-provider".into()).await?;
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(codex_state::state_db_path(&dir.path().join("state"))),
    )
    .await?;
    sqlx::query("DELETE FROM agent_job_runner_instances WHERE runner_instance_id = ?")
        .bind(db.agent_job_runner_instance_id())
        .execute(&mut connection)
        .await?;
    assert_eq!(
        observer
            .reconcile_orphaned_agent_jobs_after_restart()
            .await?
            .len(),
        1
    );
    let mut active = HashMap::from([(
        worker.thread_id,
        ActiveJobItem {
            item_id: "item-0".into(),
            started_at: Instant::now(),
            status_rx: None,
        },
    )]);
    let options = JobRunnerOptions {
        max_concurrency: 1,
        spawn_config: (*turn.config).clone(),
        environments: turn.environments.clone(),
    };
    run_agent_job_loop(
        session,
        turn,
        db.clone(),
        job.id.clone(),
        options,
        CancellationToken::new(),
        &mut active,
    )
    .await?;
    assert!(active.is_empty());
    assert!(manager.get_thread(worker.thread_id).await.is_err());
    assert_eq!(
        db.get_agent_job(&job.id)
            .await?
            .unwrap()
            .last_error
            .as_deref(),
        Some(codex_state::StateRuntime::AGENT_JOB_RESTART_ERROR)
    );
    Ok(())
}

#[tokio::test]
async fn dropped_job_owner_drains_five_blocked_workers_without_manual_retry() -> anyhow::Result<()>
{
    let (_dir, db, job) = create_running_job(5).await;
    let (mut session, turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        turn.config.model_provider.clone(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    Arc::get_mut(&mut session).unwrap().services.agent_control = manager.agent_control();
    let mut workers = Vec::new();
    let mut active_items = HashMap::new();
    for index in 0..5 {
        let worker = manager.start_thread((*turn.config).clone()).await?;
        let item = format!("item-{index}");
        assert!(
            db.mark_agent_job_item_running_with_thread(
                &job.id,
                &item,
                &worker.thread_id.to_string()
            )
            .await?
        );
        active_items.insert(
            worker.thread_id,
            ActiveJobItem {
                item_id: item,
                started_at: Instant::now(),
                status_rx: None,
            },
        );
        workers.push(worker);
    }
    let unrelated = manager.start_thread((*turn.config).clone()).await?;
    // Held active-turn guards provide controlled shutdown contention so the test
    // verifies retained cleanup ownership.
    let mut guards = Vec::new();
    for worker in &workers {
        guards.push(worker.thread.codex.session.active_turn.lock().await);
    }
    drop(AgentJobCleanupOwner {
        session: session.clone(),
        db: db.clone(),
        job_id: job.id.clone(),
        active_items,
        armed: true,
    });
    timeout(Duration::from_secs(5), async {
        loop {
            let ops = manager.captured_ops();
            if workers.iter().all(|worker| {
                ops.iter().any(|(id, op)| {
                    *id == worker.thread_id && matches!(op, codex_protocol::protocol::Op::Shutdown)
                })
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    assert_eq!(db.get_agent_job_progress(&job.id).await?.running_items, 5);
    assert!(manager.get_thread(unrelated.thread_id).await.is_ok());
    drop(guards);
    timeout(Duration::from_secs(15), async {
        loop {
            if db
                .get_agent_job_progress(&job.id)
                .await
                .unwrap()
                .running_items
                == 0
                && tokio::fs::try_exists(&job.output_csv_path).await.unwrap()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(db.get_agent_job_progress(&job.id).await?.failed_items, 5);
    assert!(manager.get_thread(unrelated.thread_id).await.is_ok());
    manager
        .agent_control()
        .shutdown_live_agent(unrelated.thread_id)
        .await?;
    Ok(())
}
