use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

async fn create_running_job(
    item_count: usize,
) -> (
    tempfile::TempDir,
    Arc<codex_state::StateRuntime>,
    codex_state::AgentJob,
) {
    create_running_job_with_schema(item_count, None).await
}

async fn create_running_job_with_schema(
    item_count: usize,
    output_schema_json: Option<Value>,
) -> (
    tempfile::TempDir,
    Arc<codex_state::StateRuntime>,
    codex_state::AgentJob,
) {
    let tempdir = tempfile::tempdir().expect("create tempdir");
    let db =
        codex_state::StateRuntime::init(tempdir.path().join("state"), "test-provider".to_string())
            .await
            .expect("initialize state runtime");
    let job_id = Uuid::new_v4().to_string();
    let input_csv_path = tempdir.path().join("input.csv");
    let output_csv_path = tempdir.path().join("output.csv");
    let items = (0..item_count)
        .map(|index| codex_state::AgentJobItemCreateParams {
            item_id: format!("item-{index}"),
            row_index: i64::try_from(index).expect("test row index should fit in i64"),
            source_id: None,
            row_json: json!({"value": format!("row-{index}")}),
        })
        .collect::<Vec<_>>();
    db.create_agent_job(
        &codex_state::AgentJobCreateParams {
            id: job_id.clone(),
            name: "test-job".to_string(),
            instruction: "Process {value}".to_string(),
            auto_export: true,
            max_runtime_seconds: None,
            output_schema_json,
            input_headers: vec!["value".to_string()],
            input_csv_path: input_csv_path.to_string_lossy().into_owned(),
            output_csv_path: output_csv_path.to_string_lossy().into_owned(),
        },
        &items,
    )
    .await
    .expect("create agent job");
    db.mark_agent_job_running(job_id.as_str())
        .await
        .expect("mark agent job running");
    let job = db
        .get_agent_job(job_id.as_str())
        .await
        .expect("load agent job")
        .expect("agent job should exist");
    (tempdir, db, job)
}

async fn create_reporting_session(
    db: Arc<codex_state::StateRuntime>,
    thread_id: ThreadId,
) -> Arc<Session> {
    let (mut session, _turn) = crate::session::tests::make_session_and_context().await;
    session.services.state_db = Some(db);
    session.thread_id = thread_id;
    Arc::new(session)
}

#[test]
fn parse_csv_supports_quotes_and_commas() {
    let input = "id,name\n1,\"alpha, beta\"\n2,gamma\n";
    let (headers, rows) = parse_csv(input).expect("csv parse");
    assert_eq!(headers, vec!["id".to_string(), "name".to_string()]);
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "alpha, beta".to_string()],
            vec!["2".to_string(), "gamma".to_string()]
        ]
    );
}

#[test]
fn csv_escape_quotes_when_needed() {
    assert_eq!(csv_escape("simple"), "simple");
    assert_eq!(csv_escape("a,b"), "\"a,b\"");
    assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
}

#[test]
fn render_instruction_template_expands_placeholders_and_escapes_braces() {
    let row = json!({
        "path": "src/lib.rs",
        "area": "test",
        "file path": "docs/readme.md",
    });
    let rendered = render_instruction_template(
        "Review {path} in {area}. Also see {file path}. Use {{literal}}.",
        &row,
    );
    assert_eq!(
        rendered,
        "Review src/lib.rs in test. Also see docs/readme.md. Use {literal}."
    );
}

#[test]
fn render_instruction_template_leaves_unknown_placeholders() {
    let row = json!({
        "path": "src/lib.rs",
    });
    let rendered = render_instruction_template("Check {path} then {missing}", &row);
    assert_eq!(rendered, "Check src/lib.rs then {missing}");
}

#[test]
fn render_instruction_template_does_not_reinterpret_replacements_or_sentinels() {
    let row = json!({
        "a": "{b}",
        "b": "secret",
        "marker": "__CODEX_CLOSE_BRACE__",
    });
    let rendered = render_instruction_template("{a} {b} __CODEX_OPEN_BRACE__ {marker}", &row);
    assert_eq!(
        rendered,
        "{b} secret __CODEX_OPEN_BRACE__ __CODEX_CLOSE_BRACE__"
    );
}

#[test]
fn ensure_unique_headers_rejects_duplicates() {
    let headers = vec!["path".to_string(), "path".to_string()];
    let Err(err) = ensure_unique_headers(headers.as_slice()) else {
        panic!("expected duplicate header error");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("csv header path is duplicated".to_string())
    );
}

#[test]
fn ensure_unique_headers_rejects_generated_output_column_collisions() {
    let headers = vec!["path".to_string(), "result_json".to_string()];
    let Err(err) = ensure_unique_headers(headers.as_slice()) else {
        panic!("expected generated output column collision error");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "csv header result_json conflicts with a generated output column".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_rejects_invalid_and_non_object_output_schemas_before_reading_csv() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    crate::session::multi_agents::update_spawn_authorization_from_text(
        &turn,
        "Use subagents to process the CSV.",
    );
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let invalid_schema_arguments = json!({
        "csv_path": "missing.csv",
        "instruction": "Process {value}",
        "output_schema": {"type": "not-a-json-schema-type"},
    })
    .to_string();
    let Err(invalid_schema_error) = spawn_agents_on_csv::handle(
        session.clone(),
        turn.clone(),
        invalid_schema_arguments,
        CancellationToken::new(),
    )
    .await
    else {
        panic!("expected invalid output schema to be rejected");
    };
    let FunctionCallError::RespondToModel(invalid_schema_message) = invalid_schema_error else {
        panic!("expected a model-facing invalid schema error");
    };
    assert!(
        invalid_schema_message.starts_with("output_schema is not a valid JSON Schema:"),
        "unexpected invalid schema error: {invalid_schema_message}"
    );

    let non_object_schema_arguments = json!({
        "csv_path": "missing.csv",
        "instruction": "Process {value}",
        "output_schema": {"type": "array"},
    })
    .to_string();
    let Err(non_object_schema_error) = spawn_agents_on_csv::handle(
        session,
        turn,
        non_object_schema_arguments,
        CancellationToken::new(),
    )
    .await
    else {
        panic!("expected a schema excluding objects to be rejected");
    };
    assert_eq!(
        non_object_schema_error,
        FunctionCallError::RespondToModel(
            "output_schema root type must allow JSON object results".to_string()
        )
    );
}

#[tokio::test]
async fn report_rejects_schema_invalid_result_without_completing_then_accepts_correction() {
    let output_schema = json!({
        "type": "object",
        "properties": {
            "score": {"type": "integer"}
        },
        "required": ["score"],
        "additionalProperties": false
    });
    let (_tempdir, db, job) =
        create_running_job_with_schema(/*item_count*/ 1, Some(output_schema)).await;
    let item = db
        .get_agent_job_item(job.id.as_str(), "item-0")
        .await
        .expect("load pending job item")
        .expect("job item should exist");
    let prompt = build_worker_prompt(&job, &item).expect("build worker prompt");
    assert!(
        prompt.contains("If the tool rejects your result, correct the payload and call it again.")
    );
    assert!(!prompt.contains("exactly once"));

    let reporting_thread_id = ThreadId::new();
    assert!(
        db.mark_agent_job_item_running_with_thread(
            job.id.as_str(),
            "item-0",
            reporting_thread_id.to_string().as_str(),
        )
        .await
        .expect("bind running job item")
    );
    let (mut session, _turn) = crate::session::tests::make_session_and_context().await;
    session.services.state_db = Some(db.clone());
    session.thread_id = reporting_thread_id;
    let session = Arc::new(session);

    let invalid_report_arguments = json!({
        "job_id": job.id.as_str(),
        "item_id": "item-0",
        "result": {"score": "high"},
    })
    .to_string();
    let Err(invalid_report_error) =
        report_agent_job_result::handle(session.clone(), invalid_report_arguments).await
    else {
        panic!("expected schema-invalid report to be rejected");
    };
    let FunctionCallError::RespondToModel(invalid_report_message) = invalid_report_error else {
        panic!("expected a model-facing invalid result error");
    };
    assert!(
        invalid_report_message.contains("$/score"),
        "validation error should identify the failing instance path: {invalid_report_message}"
    );
    let item = db
        .get_agent_job_item(job.id.as_str(), "item-0")
        .await
        .expect("reload rejected job item")
        .expect("job item should exist");
    assert_eq!(item.status, codex_state::AgentJobItemStatus::Running);
    assert_eq!(item.result_json, None);

    report_agent_job_result::handle(
        session,
        json!({
            "job_id": job.id.as_str(),
            "item_id": "item-0",
            "result": {"score": 5},
        })
        .to_string(),
    )
    .await
    .expect("schema-conforming correction should be accepted");
    let item = db
        .get_agent_job_item(job.id.as_str(), "item-0")
        .await
        .expect("reload completed job item")
        .expect("job item should exist");
    assert_eq!(item.status, codex_state::AgentJobItemStatus::Completed);
    assert_eq!(item.result_json, Some(json!({"score": 5})));
}

#[tokio::test]
async fn wait_for_status_change_blocks_after_non_final_update_is_consumed() {
    let (status_tx, status_rx) = tokio::sync::watch::channel(AgentStatus::PendingInit);
    let thread_id = ThreadId::new();
    let mut active_items = HashMap::from([(
        thread_id,
        ActiveJobItem {
            item_id: "item-1".to_string(),
            started_at: Instant::now(),
            status_rx: Some(status_rx),
        },
    )]);

    status_tx
        .send(AgentStatus::Running)
        .expect("status receiver should remain open");
    let observed = active_item_watch_status(
        active_items
            .get_mut(&thread_id)
            .expect("active item should exist"),
    );
    assert_eq!(observed, Some(AgentStatus::Running));

    let wait = wait_for_status_change(&active_items);
    tokio::pin!(wait);
    assert!(futures::poll!(&mut wait).is_pending());

    status_tx
        .send(AgentStatus::Interrupted)
        .expect("status receiver should remain open");
    timeout(Duration::from_secs(1), &mut wait)
        .await
        .expect("a genuinely newer status should wake the waiter");
}

#[tokio::test]
async fn atomic_csv_write_replaces_existing_destination() {
    let tempdir = tempfile::tempdir().expect("create tempdir");
    let output_path = tempdir.path().join("output.csv");
    std::fs::write(&output_path, "old contents").expect("seed destination");

    write_job_csv_atomically(output_path.clone(), "header\nnew value\n".to_string())
        .await
        .expect("replace destination atomically");

    assert_eq!(
        std::fs::read_to_string(output_path).expect("read replaced destination"),
        "header\nnew value\n"
    );
}

#[tokio::test]
async fn atomic_csv_write_failure_leaves_no_partial_destination() {
    let tempdir = tempfile::tempdir().expect("create tempdir");
    let output_path = tempdir.path().join("x".repeat(300));

    write_job_csv_atomically(output_path.clone(), "partial csv contents".to_string())
        .await
        .expect_err("overlong destination name should fail publication");

    assert!(!output_path.exists());
    assert_eq!(
        std::fs::read_dir(tempdir.path())
            .expect("read tempdir")
            .count(),
        0,
        "failed publication should remove its temporary file"
    );
}

#[tokio::test]
async fn durability_regression_restart_reconciles_and_exports_partial_csv() {
    let (tempdir, owner_db, job) = create_running_job(/*item_count*/ 2).await;
    let completed_thread_id = ThreadId::new().to_string();
    assert!(
        owner_db
            .mark_agent_job_item_running_with_thread(
                job.id.as_str(),
                "item-0",
                completed_thread_id.as_str(),
            )
            .await
            .expect("bind completed item")
    );
    assert!(
        owner_db
            .report_agent_job_item_result(
                job.id.as_str(),
                "item-0",
                completed_thread_id.as_str(),
                &json!({"result": "kept"}),
            )
            .await
            .expect("complete first item")
    );

    let db =
        codex_state::StateRuntime::init(tempdir.path().join("state"), "test-provider".to_string())
            .await
            .expect("initialize parallel state runtime");
    reconcile_orphaned_agent_jobs_after_restart(db.clone())
        .await
        .expect("preserve job owned by a live parallel runtime");
    assert_eq!(
        db.get_agent_job(job.id.as_str())
            .await
            .expect("load live foreign job")
            .expect("live foreign job should exist")
            .status,
        codex_state::AgentJobStatus::Running
    );

    owner_db.close().await;
    reconcile_orphaned_agent_jobs_after_restart(db.clone())
        .await
        .expect("reconcile restarted job and export its partial snapshot");

    let stored_job = db
        .get_agent_job(job.id.as_str())
        .await
        .expect("load reconciled job")
        .expect("reconciled job should exist");
    assert_eq!(stored_job.status, codex_state::AgentJobStatus::Failed);
    assert_eq!(
        stored_job.last_error.as_deref(),
        Some(codex_state::StateRuntime::AGENT_JOB_RESTART_ERROR)
    );
    let progress = db
        .get_agent_job_progress(job.id.as_str())
        .await
        .expect("load reconciled progress");
    assert_eq!(progress.completed_items, 1);
    assert_eq!(progress.failed_items, 1);
    assert_eq!(progress.pending_items, 0);
    assert_eq!(progress.running_items, 0);

    let csv = tokio::fs::read_to_string(job.output_csv_path.as_str())
        .await
        .expect("read reconciled partial csv");
    assert!(csv.contains("completed"));
    assert!(csv.contains("kept"));
    assert!(csv.contains(codex_state::StateRuntime::AGENT_JOB_RESTART_ERROR));
}

#[tokio::test]
async fn runner_settles_non_limit_spawn_failure_without_retrying() {
    let (_tempdir, db, job) = create_running_job(/*item_count*/ 1).await;
    let (session, turn, _events) = crate::session::tests::make_session_and_context_with_rx().await;
    let options = JobRunnerOptions {
        max_concurrency: 1,
        spawn_config: (*turn.config).clone(),
    };

    timeout(
        Duration::from_secs(5),
        run_agent_job_loop(
            session,
            turn,
            db.clone(),
            job.id.clone(),
            options,
            CancellationToken::new(),
            &mut HashMap::new(),
        ),
    )
    .await
    .expect("permanent spawn failure should not retry forever")
    .expect("runner should settle a permanent spawn failure");

    let item = db
        .get_agent_job_item(job.id.as_str(), "item-0")
        .await
        .expect("load job item")
        .expect("job item should exist");
    assert_eq!(item.status, codex_state::AgentJobItemStatus::Failed);
    assert!(
        item.last_error
            .as_deref()
            .is_some_and(|error| error.starts_with("failed to spawn worker:"))
    );
    let progress = db
        .get_agent_job_progress(job.id.as_str())
        .await
        .expect("load job progress");
    assert_eq!(progress.pending_items, 0);
    assert_eq!(progress.running_items, 0);
    assert_eq!(progress.failed_items, 1);
}

#[tokio::test]
async fn parent_cancellation_settles_running_item_and_exports_snapshot() {
    let (_tempdir, db, job) = create_running_job(/*item_count*/ 1).await;
    let assigned_thread_id = ThreadId::new();
    assert!(
        db.mark_agent_job_item_running_with_thread(
            job.id.as_str(),
            "item-0",
            assigned_thread_id.to_string().as_str(),
        )
        .await
        .expect("bind running job item")
    );
    let (mut session, turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        turn.config.model_provider.clone(),
    );
    Arc::get_mut(&mut session)
        .expect("unique session")
        .services
        .agent_control = manager.agent_control();
    let options = JobRunnerOptions {
        max_concurrency: 1,
        spawn_config: (*turn.config).clone(),
    };
    let cancellation_token = CancellationToken::new();
    cancellation_token.cancel();

    timeout(
        Duration::from_secs(5),
        run_agent_job_loop(
            session,
            turn,
            db.clone(),
            job.id.clone(),
            options,
            cancellation_token,
            &mut HashMap::new(),
        ),
    )
    .await
    .expect("cancelled runner should terminate promptly")
    .expect("cancelled runner should tear down and export");

    let stored_job = db
        .get_agent_job(job.id.as_str())
        .await
        .expect("load cancelled job")
        .expect("cancelled job should exist");
    assert_eq!(stored_job.status, codex_state::AgentJobStatus::Cancelled);
    let progress = db
        .get_agent_job_progress(job.id.as_str())
        .await
        .expect("load cancelled job progress");
    assert_eq!(progress.running_items, 0);
    assert_eq!(progress.failed_items, 1);
    assert!(
        tokio::fs::try_exists(&job.output_csv_path)
            .await
            .expect("check exported snapshot")
    );
}

#[tokio::test]
async fn worker_stop_cancels_job_settles_other_worker_and_requests_shutdown() {
    let (_tempdir, db, job) = create_running_job(/*item_count*/ 2).await;
    let reporting_thread_id = ThreadId::new();
    let (mut runner_session, turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        turn.config.model_provider.clone(),
    );
    let other_worker_thread_id = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("start other worker thread")
        .thread_id;
    Arc::get_mut(&mut runner_session)
        .expect("runner session should be uniquely owned")
        .services
        .agent_control = manager.agent_control();

    assert!(
        db.mark_agent_job_item_running_with_thread(
            job.id.as_str(),
            "item-0",
            reporting_thread_id.to_string().as_str(),
        )
        .await
        .expect("bind reporting worker")
    );
    assert!(
        db.mark_agent_job_item_running_with_thread(
            job.id.as_str(),
            "item-1",
            other_worker_thread_id.to_string().as_str(),
        )
        .await
        .expect("bind other worker")
    );

    let stop_output = report_agent_job_result::handle(
        create_reporting_session(db.clone(), reporting_thread_id).await,
        json!({
            "job_id": job.id.as_str(),
            "item_id": "item-0",
            "result": {"reason": "stop"},
            "stop": true,
        })
        .to_string(),
    )
    .await
    .expect("reporting worker should be able to cancel the job");
    assert_eq!(stop_output.into_text(), r#"{"accepted":true}"#);

    let late_output = report_agent_job_result::handle(
        create_reporting_session(db.clone(), other_worker_thread_id).await,
        json!({
            "job_id": job.id.as_str(),
            "item_id": "item-1",
            "result": {"late": true},
        })
        .to_string(),
    )
    .await
    .expect("late report should return an explicit rejection");
    assert_eq!(late_output.into_text(), r#"{"accepted":false}"#);

    let options = JobRunnerOptions {
        max_concurrency: 2,
        spawn_config: (*turn.config).clone(),
    };
    timeout(
        Duration::from_secs(5),
        run_agent_job_loop(
            runner_session,
            turn,
            db.clone(),
            job.id.clone(),
            options,
            CancellationToken::new(),
            &mut HashMap::new(),
        ),
    )
    .await
    .expect("worker-requested cancellation should terminate the runner promptly")
    .expect("cancelled runner should settle workers and export");

    let stored_job = db
        .get_agent_job(job.id.as_str())
        .await
        .expect("load cancelled job")
        .expect("cancelled job should exist");
    assert_eq!(stored_job.status, codex_state::AgentJobStatus::Cancelled);
    let reported_item = db
        .get_agent_job_item(job.id.as_str(), "item-0")
        .await
        .expect("load reporting item")
        .expect("reporting item should exist");
    assert_eq!(
        reported_item.status,
        codex_state::AgentJobItemStatus::Completed
    );
    let stopped_item = db
        .get_agent_job_item(job.id.as_str(), "item-1")
        .await
        .expect("load stopped item")
        .expect("stopped item should exist");
    assert_eq!(stopped_item.status, codex_state::AgentJobItemStatus::Failed);
    assert_eq!(stopped_item.result_json, None);
    let progress = db
        .get_agent_job_progress(job.id.as_str())
        .await
        .expect("load cancelled job progress");
    assert_eq!(progress.completed_items, 1);
    assert_eq!(progress.failed_items, 1);
    assert_eq!(progress.running_items, 0);
    assert!(
        matches!(
            manager.get_thread(other_worker_thread_id).await,
            Err(CodexErr::ThreadNotFound(id)) if id == other_worker_thread_id
        ),
        "successful job cleanup must remove the terminated worker"
    );
    assert!(manager.captured_ops().into_iter().any(|(thread_id, op)| {
        thread_id == other_worker_thread_id && matches!(op, codex_protocol::protocol::Op::Shutdown)
    }));
    assert!(
        tokio::fs::try_exists(&job.output_csv_path)
            .await
            .expect("check exported snapshot")
    );
}

#[derive(Clone, Copy)]
enum BlockedJobCleanupRoute {
    RecoveredTimeout,
    ActiveTimeout,
    FinishedWorker,
    ParentCancellation,
}

async fn assert_runner_preserves_worker_until_cleanup(route: BlockedJobCleanupRoute) {
    use sqlx::Connection;

    let item_count = if matches!(route, BlockedJobCleanupRoute::ParentCancellation) {
        2
    } else {
        1
    };
    let (tempdir, db, job) = create_running_job(item_count).await;
    let (mut session, turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        turn.config.model_provider.clone(),
    );
    let worker = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("start actual worker");
    let thread_id = worker.thread_id;
    Arc::get_mut(&mut session)
        .expect("unique runner session")
        .services
        .agent_control = manager.agent_control();
    assert!(
        db.mark_agent_job_item_running_with_thread(&job.id, "item-0", &thread_id.to_string())
            .await
            .expect("bind worker")
    );
    let other_worker = if matches!(route, BlockedJobCleanupRoute::ParentCancellation) {
        let other = manager
            .start_thread((*turn.config).clone())
            .await
            .expect("start other worker");
        assert!(
            db.mark_agent_job_item_running_with_thread(
                &job.id,
                "item-1",
                &other.thread_id.to_string()
            )
            .await
            .expect("bind other worker")
        );
        Some(other)
    } else {
        None
    };
    let sqlite_options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(codex_state::state_db_path(&tempdir.path().join("state")));
    let mut connection = sqlx::SqliteConnection::connect_with(&sqlite_options)
        .await
        .expect("open fault connection");
    match route {
        BlockedJobCleanupRoute::RecoveredTimeout => {
            sqlx::query("UPDATE agent_jobs SET max_runtime_seconds = 1 WHERE id = ?")
                .bind(&job.id)
                .execute(&mut connection)
                .await
                .expect("set persisted runtime");
            sqlx::query("UPDATE agent_job_items SET updated_at = 0 WHERE job_id = ?")
                .bind(&job.id)
                .execute(&mut connection)
                .await
                .expect("make recovery item stale");
        }
        BlockedJobCleanupRoute::ActiveTimeout => {
            assert!(
                !is_final(&worker.thread.agent_status().await),
                "active-timeout scenario must enter the active runner loop"
            );
            sqlx::query("UPDATE agent_jobs SET max_runtime_seconds = 1 WHERE id = ?")
                .bind(&job.id)
                .execute(&mut connection)
                .await
                .expect("set runtime");
            // Persisted freshness keeps recovery from taking the stale-item path.
            sqlx::query("UPDATE agent_job_items SET updated_at = ? WHERE job_id = ?")
                .bind(chrono::Utc::now().timestamp() + 60)
                .bind(&job.id)
                .execute(&mut connection)
                .await
                .expect("keep recovery item fresh");
        }
        BlockedJobCleanupRoute::FinishedWorker => {
            let worker_turn = worker.thread.codex.session.new_default_turn().await;
            worker
                .thread
                .codex
                .session
                .send_event(
                    worker_turn.as_ref(),
                    codex_protocol::protocol::EventMsg::TurnComplete(
                        codex_protocol::protocol::TurnCompleteEvent {
                            surfaced_result: None,
                            turn_id: worker_turn.sub_id.clone(),
                            last_agent_message: Some("finished without reporting".to_string()),
                            error: None,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                            timing: None,
                        },
                    ),
                )
                .await;
            assert!(is_final(&worker.thread.agent_status().await));
        }
        BlockedJobCleanupRoute::ParentCancellation => {}
    }
    // Shutdown must acquire this real session lock to schedule turn termination.
    // Unlike the terminal-task wait, this boundary has no competing ten-second timeout.
    let shutdown_guard = worker.thread.codex.session.active_turn.lock().await;
    let other_shutdown_guard = if let Some(other) = other_worker.as_ref() {
        Some(other.thread.codex.session.active_turn.lock().await)
    } else {
        None
    };
    let cancellation = CancellationToken::new();
    if matches!(route, BlockedJobCleanupRoute::ParentCancellation) {
        cancellation.cancel();
    }
    let options = JobRunnerOptions {
        max_concurrency: 1,
        spawn_config: (*turn.config).clone(),
    };
    let runner = tokio::spawn({
        let session = session.clone();
        let db = db.clone();
        let job_id = job.id.clone();
        async move {
            let mut active_items = HashMap::new();
            let result = run_agent_job_loop(
                session,
                turn,
                db,
                job_id,
                options,
                cancellation,
                &mut active_items,
            )
            .await;
            (result, active_items)
        }
    });
    timeout(Duration::from_secs(5), async {
        loop {
            if manager.captured_ops().iter().any(|(id, op)| {
                (*id == thread_id
                    || other_worker
                        .as_ref()
                        .is_some_and(|other| *id == other.thread_id))
                    && matches!(op, codex_protocol::protocol::Op::Shutdown)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("normal runner must request worker shutdown");
    let item = db
        .get_agent_job_item(&job.id, "item-0")
        .await
        .expect("read pending cleanup item")
        .expect("item exists");
    assert_eq!(item.status, codex_state::AgentJobItemStatus::Running);
    assert_eq!(
        item.assigned_thread_id.as_deref(),
        Some(thread_id.to_string().as_str())
    );
    assert!(
        !runner.is_finished(),
        "cleanup cannot succeed while terminal work remains"
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::time::resume();
    if let Some(other) = other_worker.as_ref() {
        timeout(Duration::from_secs(5), async {
            loop {
                assert!(
                    !runner.is_finished(),
                    "one failed shutdown must not skip another worker"
                );
                let ops = manager.captured_ops();
                if [thread_id, other.thread_id].iter().all(|expected| {
                    ops.iter().any(|(id, op)| {
                        id == expected && matches!(op, codex_protocol::protocol::Op::Shutdown)
                    })
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("cleanup must attempt both workers regardless of iteration order");
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::time::resume();
    }
    let (result, mut active_items) = timeout(Duration::from_secs(5), runner)
        .await
        .expect("shutdown deadline returns")
        .expect("runner joins");
    let error = result
        .expect_err("unfinished worker cleanup must be reported")
        .to_string();
    assert!(
        error.contains("timed out") && error.contains(&thread_id.to_string()),
        "{error}"
    );
    assert_eq!(
        active_items
            .get(&thread_id)
            .expect("retain caller cleanup ownership")
            .item_id,
        "item-0"
    );
    assert!(
        manager.get_thread(thread_id).await.is_ok(),
        "late cleanup retains actual worker ownership"
    );
    let item = db
        .get_agent_job_item(&job.id, "item-0")
        .await
        .expect("read retained binding")
        .expect("item exists");
    assert_eq!(item.status, codex_state::AgentJobItemStatus::Running);
    assert_eq!(
        item.assigned_thread_id.as_deref(),
        Some(thread_id.to_string().as_str())
    );
    if let Some(other) = other_worker.as_ref() {
        assert!(
            error.contains(&other.thread_id.to_string()),
            "each failed shutdown must be reported: {error}"
        );
        assert!(manager.get_thread(other.thread_id).await.is_ok());
        let pending = db
            .get_agent_job_item(&job.id, "item-1")
            .await
            .expect("read other item")
            .expect("other item exists");
        assert_eq!(pending.status, codex_state::AgentJobItemStatus::Running);
        assert_eq!(
            pending.assigned_thread_id.as_deref(),
            Some(other.thread_id.to_string().as_str())
        );
        assert_eq!(pending.result_json, None);
        assert_eq!(active_items.len(), 2);
    }
    let stored_job = db
        .get_agent_job(&job.id)
        .await
        .expect("load job")
        .expect("job exists");
    assert_ne!(stored_job.status, codex_state::AgentJobStatus::Completed);
    if !matches!(route, BlockedJobCleanupRoute::ParentCancellation) {
        assert!(
            !tokio::fs::try_exists(&job.output_csv_path)
                .await
                .expect("check forbidden successful export")
        );
    }
    drop(shutdown_guard);
    drop(other_shutdown_guard);
    timeout(
        Duration::from_secs(5),
        worker.thread.wait_until_terminated(),
    )
    .await
    .expect("worker actually terminates");
    timeout(Duration::from_secs(5), async {
        while manager.get_thread(thread_id).await.is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late owner removes terminated worker");
    if let Some(other) = other_worker.as_ref() {
        timeout(Duration::from_secs(5), other.thread.wait_until_terminated())
            .await
            .expect("other worker actually terminates");
        timeout(Duration::from_secs(5), async {
            while manager.get_thread(other.thread_id).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("late owner removes other worker");
    }
    terminate_agent_job_workers(
        session,
        db.clone(),
        &job.id,
        &mut active_items,
        "retry after worker termination",
    )
    .await
    .expect("cleanup retry settles retained item");
    assert!(active_items.is_empty());
    if other_worker.is_some() {
        let item = db
            .get_agent_job_item(&job.id, "item-1")
            .await
            .expect("load other settled item")
            .expect("item exists");
        assert_eq!(item.status, codex_state::AgentJobItemStatus::Failed);
        assert_eq!(item.assigned_thread_id, None);
    }
    let item = db
        .get_agent_job_item(&job.id, "item-0")
        .await
        .expect("load settled item")
        .expect("item exists");
    assert_eq!(item.status, codex_state::AgentJobItemStatus::Failed);
    assert_eq!(item.assigned_thread_id, None);
    assert_eq!(item.result_json, None);
    assert_eq!(
        db.get_agent_job_progress(&job.id)
            .await
            .expect("load settled progress")
            .running_items,
        0
    );
}

#[tokio::test]
async fn runner_recovered_timeout_retains_worker_until_cleanup() {
    assert_runner_preserves_worker_until_cleanup(BlockedJobCleanupRoute::RecoveredTimeout).await;
}

#[tokio::test]
async fn runner_active_timeout_retains_worker_until_cleanup() {
    assert_runner_preserves_worker_until_cleanup(BlockedJobCleanupRoute::ActiveTimeout).await;
}

#[tokio::test]
async fn runner_finished_item_retains_worker_until_cleanup() {
    assert_runner_preserves_worker_until_cleanup(BlockedJobCleanupRoute::FinishedWorker).await;
}

#[tokio::test]
async fn runner_cancellation_retains_worker_until_cleanup() {
    assert_runner_preserves_worker_until_cleanup(BlockedJobCleanupRoute::ParentCancellation).await;
}

use sqlx::Connection as _;

struct CsvJobSqlFaultFixture {
    _tempdir: tempfile::TempDir,
    db: Arc<codex_state::StateRuntime>,
    connection: sqlx::SqliteConnection,
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    arguments: String,
    output_path: PathBuf,
}

async fn csv_job_sql_fault_fixture(
    mode: MultiAgentVersion,
) -> anyhow::Result<CsvJobSqlFaultFixture> {
    let tempdir = tempfile::tempdir()?;
    let sqlite_home = tempdir.path().join("state");
    let db =
        codex_state::StateRuntime::init(sqlite_home.clone(), "test-provider".to_string()).await?;
    let connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(codex_state::state_db_path(&sqlite_home)),
    )
    .await?;
    let input_path = tempdir.path().join("input.csv");
    let output_path = tempdir.path().join("output.csv");
    tokio::fs::write(&input_path, "value\nfirst\n").await?;
    let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
    session.services.state_db = Some(db.clone());
    // The default unavailable AgentControl is the real failed-spawn prerequisite used by
    // runner_settles_non_limit_spawn_failure_without_retrying; no successful worker is faked.
    turn.multi_agent_version = mode;
    crate::session::multi_agents::update_spawn_authorization_from_text(
        &turn,
        "Use subagents to process the CSV.",
    );
    if mode == MultiAgentVersion::V2 {
        assert!(crate::session::multi_agents::spawn_is_authorized(&turn));
    }
    let arguments = json!({
        "csv_path": input_path.to_str().expect("temporary input path must be UTF-8"),
        "output_csv_path": output_path.to_str().expect("temporary output path must be UTF-8"),
        "instruction": "Process {value}",
        "max_concurrency": 1,
    })
    .to_string();
    Ok(CsvJobSqlFaultFixture {
        _tempdir: tempdir,
        db,
        connection,
        session: Arc::new(session),
        turn: Arc::new(turn),
        arguments,
        output_path,
    })
}

async fn csv_job_fault_handler_error(fixture: &CsvJobSqlFaultFixture) -> String {
    match spawn_agents_on_csv::handle(
        fixture.session.clone(),
        fixture.turn.clone(),
        fixture.arguments.clone(),
        CancellationToken::new(),
    )
    .await
    {
        Err(error) => error.to_string(),
        Ok(output) => panic!(
            "database fault must remain visible as a tool error, got {}",
            output.into_text()
        ),
    }
}

async fn assert_no_csv_worker_binding_or_result(
    connection: &mut sqlx::SqliteConnection,
) -> anyhow::Result<()> {
    let (bound_items, reported_items): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(assigned_thread_id IS NOT NULL), 0), \
         COALESCE(SUM(result_json IS NOT NULL), 0) FROM agent_job_items",
    )
    .fetch_one(&mut *connection)
    .await?;
    assert_eq!((bound_items, reported_items), (0, 0));
    let persisted_threads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threads")
        .fetch_one(&mut *connection)
        .await?;
    assert_eq!(
        persisted_threads, 0,
        "failed admission must not persist a worker thread"
    );
    Ok(())
}

#[tokio::test]
async fn csv_setup_error_reports_failed_job_persistence() -> anyhow::Result<()> {
    let mut fixture = csv_job_sql_fault_fixture(MultiAgentVersion::Disabled).await?;
    sqlx::query(
        "CREATE TRIGGER reject_job_failure BEFORE UPDATE OF status ON agent_jobs \
         WHEN NEW.status = 'failed' \
         BEGIN SELECT RAISE(FAIL, 'injected terminal write'); END",
    )
    .execute(&mut fixture.connection)
    .await?;

    let error = csv_job_fault_handler_error(&fixture).await;
    assert!(error.contains("multi-agent runtime is disabled"), "{error}");
    assert!(error.contains("injected terminal write"), "{error}");
    let jobs: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT id, status, last_error FROM agent_jobs")
            .fetch_all(&mut fixture.connection)
            .await?;
    assert_eq!(
        jobs.len(),
        1,
        "the fault must occur after actual job creation"
    );
    assert_eq!(jobs[0].1, "pending");
    assert_eq!(jobs[0].2, None);
    assert_no_csv_worker_binding_or_result(&mut fixture.connection).await?;
    assert!(!tokio::fs::try_exists(&fixture.output_path).await?);

    fixture.connection.close().await?;
    fixture.db.close().await;
    Ok(())
}

#[tokio::test]
async fn csv_runner_error_preserves_terminal_persistence_failure() -> anyhow::Result<()> {
    let mut fixture = csv_job_sql_fault_fixture(MultiAgentVersion::V2).await?;
    sqlx::query(
        "CREATE TRIGGER reject_item_failure BEFORE UPDATE OF status ON agent_job_items \
         WHEN NEW.status = 'failed' \
         BEGIN SELECT RAISE(FAIL, 'injected item write'); END",
    )
    .execute(&mut fixture.connection)
    .await?;
    sqlx::query(
        "CREATE TRIGGER reject_job_failure BEFORE UPDATE OF status ON agent_jobs \
         WHEN NEW.status = 'failed' \
         BEGIN SELECT RAISE(FAIL, 'injected terminal write'); END",
    )
    .execute(&mut fixture.connection)
    .await?;

    let error = csv_job_fault_handler_error(&fixture).await;
    assert!(error.contains("injected item write"), "{error}");
    assert!(error.contains("injected terminal write"), "{error}");
    let jobs: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT id, status, last_error FROM agent_jobs")
            .fetch_all(&mut fixture.connection)
            .await?;
    assert_eq!(jobs.len(), 1);
    assert!(
        error.contains(&jobs[0].0),
        "the error must identify the job: {error}"
    );
    assert_eq!(
        jobs[0].1, "running",
        "failed persistence must not be reported as durable completion"
    );
    assert_eq!(jobs[0].2, None);
    let item_statuses: Vec<String> = sqlx::query_scalar("SELECT status FROM agent_job_items")
        .fetch_all(&mut fixture.connection)
        .await?;
    assert_eq!(item_statuses, vec!["pending".to_string()]);
    assert_no_csv_worker_binding_or_result(&mut fixture.connection).await?;

    fixture.connection.close().await?;
    fixture.db.close().await;
    Ok(())
}

#[tokio::test]
async fn csv_failed_detail_read_error_is_not_reported_as_missing_evidence() -> anyhow::Result<()> {
    let mut fixture = csv_job_sql_fault_fixture(MultiAgentVersion::V2).await?;
    // Export happens before the completed transition. Corrupt only the subsequent failed-item
    // read, preserving real failure text, progress counts, and the already-written snapshot.
    sqlx::query(
        "CREATE TRIGGER corrupt_failed_row_after_completion \
         AFTER UPDATE OF status ON agent_jobs WHEN NEW.status = 'completed' \
         BEGIN UPDATE agent_job_items SET row_json = 'not-json' \
         WHERE job_id = NEW.id AND status = 'failed'; END",
    )
    .execute(&mut fixture.connection)
    .await?;

    let error = csv_job_fault_handler_error(&fixture).await;
    assert!(
        error.contains("failed") && error.contains("item"),
        "{error}"
    );
    assert!(!error.contains("no error details were recorded"), "{error}");
    let jobs: Vec<(String, String)> = sqlx::query_as("SELECT id, status FROM agent_jobs")
        .fetch_all(&mut fixture.connection)
        .await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].1, "completed");
    let failed_rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT status, row_json, last_error FROM agent_job_items WHERE status = 'failed'",
    )
    .fetch_all(&mut fixture.connection)
    .await?;
    assert_eq!(failed_rows.len(), 1);
    assert_eq!(failed_rows[0].0, "failed");
    assert_eq!(failed_rows[0].1, "not-json");
    assert!(failed_rows[0].2.starts_with("failed to spawn worker:"));
    let progress = fixture.db.get_agent_job_progress(&jobs[0].0).await?;
    assert_eq!(progress.total_items, 1);
    assert_eq!(progress.failed_items, 1);
    assert_eq!(progress.completed_items, 0);
    assert!(
        fixture
            .db
            .list_agent_job_items(
                &jobs[0].0,
                Some(codex_state::AgentJobItemStatus::Failed),
                Some(5),
            )
            .await
            .is_err(),
        "the injected persisted row must exercise the actual failed-detail decoder"
    );
    let csv = tokio::fs::read_to_string(&fixture.output_path).await?;
    let (_headers, exported_rows) =
        parse_csv(&csv).expect("the pre-fault export must remain valid");
    assert_eq!(exported_rows.len(), 1);
    assert_eq!(exported_rows[0][0], "first");
    assert_eq!(exported_rows[0][5], "failed");
    assert_no_csv_worker_binding_or_result(&mut fixture.connection).await?;

    fixture.connection.close().await?;
    fixture.db.close().await;
    Ok(())
}
