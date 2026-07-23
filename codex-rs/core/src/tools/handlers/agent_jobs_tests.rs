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
    let (session, turn, _events) = crate::session::tests::make_session_and_context_with_rx().await;
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
    assert!(manager.captured_ops().into_iter().any(|(thread_id, op)| {
        thread_id == other_worker_thread_id && matches!(op, codex_protocol::protocol::Op::Shutdown)
    }));
    assert!(
        tokio::fs::try_exists(&job.output_csv_path)
            .await
            .expect("check exported snapshot")
    );
}
