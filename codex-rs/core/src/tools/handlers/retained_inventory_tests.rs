use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::read_tool_output_selectors_with_reuse;
use crate::tools::context::ToolCallSource;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_exec_server::CopyOptions;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::ExecutorFileSystemFuture;
use codex_exec_server::FileMetadata;
use codex_exec_server::FileSystemReadStream;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::ReadDirectoryEntry;
use codex_exec_server::RemoveOptions;
use codex_utils_absolute_path::test_support::PathExt;
use codex_utils_path_uri::PathUri;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Default)]
struct DelayedObservationFileSystem {
    active: AtomicUsize,
    peak: AtomicUsize,
    reads: AtomicUsize,
    read_started: tokio::sync::Notify,
    block_reads: bool,
}

struct ActiveObservation<'a>(&'a AtomicUsize);

impl Drop for ActiveObservation<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl DelayedObservationFileSystem {
    async fn delay(&self, block: bool, delay_ms: u64) {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = ActiveObservation(&self.active);
        self.peak.fetch_max(active, Ordering::SeqCst);
        if block {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
}

impl ExecutorFileSystem for DelayedObservationFileSystem {
    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async move {
            assert!(sandbox.is_some());
            self.delay(
                false,
                if path.to_string().ends_with("file-00") {
                    30
                } else {
                    5
                },
            )
            .await;
            Ok(FileMetadata {
                is_file: true,
                is_directory: false,
                is_symlink: false,
                size: 4,
                created_at_ms: 0,
                modified_at_ms: 0,
            })
        })
    }

    fn read_file_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            assert_eq!(max_bytes, 8 * 1024 * 1024);
            assert!(sandbox.is_some());
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.read_started.notify_one();
            self.delay(self.block_reads, 5).await;
            Ok(Some(path.to_string().into_bytes()))
        })
    }

    fn canonicalize<'a>(
        &'a self,
        _: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        panic!("unexpected canonicalize")
    }
    fn read_file<'a>(
        &'a self,
        _: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        panic!("unexpected unbounded read")
    }
    fn read_file_stream<'a>(
        &'a self,
        _: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        panic!("unexpected stream read")
    }
    fn write_file<'a>(
        &'a self,
        _: &'a PathUri,
        _: Vec<u8>,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected write")
    }
    fn create_directory<'a>(
        &'a self,
        _: &'a PathUri,
        _: CreateDirectoryOptions,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected mkdir")
    }
    fn read_directory<'a>(
        &'a self,
        _: &'a PathUri,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        panic!("unexpected enumeration")
    }
    fn remove<'a>(
        &'a self,
        _: &'a PathUri,
        _: RemoveOptions,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected remove")
    }
    fn copy<'a>(
        &'a self,
        _: &'a PathUri,
        _: &'a PathUri,
        _: CopyOptions,
        _: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected copy")
    }
}

async fn observation_context(
    cwd: &std::path::Path,
    fs: Arc<DelayedObservationFileSystem>,
) -> ToolInvocation {
    let mut context = context().await;
    let environments = &mut Arc::get_mut(&mut context.step_context)
        .unwrap()
        .environments
        .turn_environments;
    let original = &environments[0];
    environments[0] = crate::session::turn_context::TurnEnvironment::new(
        original.environment_id.clone(),
        Arc::new(codex_exec_server::Environment::default_for_tests_with_filesystem(fs)),
        PathUri::from_abs_path(&cwd.abs()),
        original.shell.clone(),
    );
    context
}

#[tokio::test]
async fn inventory_observation_overlaps_bounded_reads_and_preserves_order() {
    let cwd = tempfile::tempdir().unwrap();
    let fs = Arc::new(DelayedObservationFileSystem::default());
    let context = observation_context(cwd.path(), Arc::clone(&fs)).await;
    let initial = create(&context).await;
    let paths = (0..40)
        .rev()
        .map(|index| format!("file-{index:02}"))
        .collect::<Vec<_>>();
    let observed = call_through_registry(&context, json!({"operation":"observe", "inventory_id":initial["inventory_id"], "category":"entrypoint", "paths":paths, "complete":true})).await.unwrap();
    let mut records = Vec::new();
    let mut offset = 0;
    loop {
        let page = call(&context, json!({"operation":"read", "inventory_id":observed["inventory_id"], "offset":offset, "limit":50})).await.unwrap();
        records.extend(page["records"].as_array().unwrap().iter().cloned());
        let Some(next) = page["next_offset"].as_u64() else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(records.len(), 40);
    let ids = records
        .iter()
        .map(|record| record["candidate"]["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    // The read view sorts records itself; inspect producer evidence as well so
    // unordered observation completion cannot silently change its identity.
    let (source, _) = read_source(
        &context,
        &Source {
            artifact_id: observed["inventory_id"].as_str().unwrap().to_string(),
            pointer: "/categories/entrypoint/provenance/source".to_string(),
            lines: None,
        },
    )
    .await
    .unwrap();
    let (enumeration, _) = read_source(&context, &serde_json::from_value(source).unwrap())
        .await
        .unwrap();
    let observed_ids = enumeration["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|candidate| candidate["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(observed_ids, ids);
    for record in &records {
        let id = record["candidate"]["id"].as_str().unwrap();
        let path = PathUri::from_abs_path(&std::path::Path::new(id).abs());
        assert_eq!(
            record["candidate"]["revision"],
            crate::tool_history::sha256(path.to_string().as_bytes())
        );
    }
    assert_eq!(fs.reads.load(Ordering::SeqCst), 40);
    let peak = fs.peak.load(Ordering::SeqCst);
    assert!(
        peak > 1 && peak <= OBSERVATION_CONCURRENCY,
        "peak concurrency: {peak}"
    );
    assert_eq!(fs.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn inventory_cancellation_drops_pending_observations() {
    let cwd = tempfile::tempdir().unwrap();
    let fs = Arc::new(DelayedObservationFileSystem {
        block_reads: true,
        ..Default::default()
    });
    let context = observation_context(cwd.path(), Arc::clone(&fs)).await;
    let initial = create(&context).await;
    let observe = call(
        &context,
        json!({"operation":"observe", "inventory_id":initial["inventory_id"], "category":"entrypoint", "paths":["file-00", "file-01"], "complete":true}),
    );
    let cancel = async {
        loop {
            let notified = fs.read_started.notified();
            if fs.reads.load(Ordering::SeqCst) == 2 {
                break;
            }
            notified.await;
        }
        assert_eq!(fs.active.load(Ordering::SeqCst), 2);
        context.cancellation_token.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(observe, cancel)
    })
    .await
    .expect("cancelled I/O must not remain blocked");
    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("cancelled observation must fail")
    };
    assert!(
        message.contains("inventory observation cancelled"),
        "{message}"
    );
    assert_eq!(fs.active.load(Ordering::SeqCst), 0);
    assert_eq!(fs.reads.load(Ordering::SeqCst), 2);
}

async fn context() -> ToolInvocation {
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    ToolInvocation {
        session: Arc::new(session),
        step_context: StepContext::for_test(Arc::new(turn)),
        cancellation_token: Default::default(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "inventory-test".to_string(),
        tool_name: ToolName::plain("inventory"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    }
}

#[tokio::test]
async fn inventory_observes_real_files_and_detects_source_changes() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (session, turn, _events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test"),
            Vec::new(),
            home.path(),
            |config| {
                config.cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                    workspace.path(),
                )
                .unwrap();
            },
        )
        .await;
    let mut turn = Arc::try_unwrap(turn).ok().unwrap();
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    let context = ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::new(turn)),
        ..context().await
    };
    let cwd = context
        .step_context
        .environments
        .primary()
        .unwrap()
        .cwd()
        .to_abs_path()
        .unwrap();
    let tracked = cwd.as_path().join("tracked.txt");
    let untracked = cwd.as_path().join("untracked.txt");
    let absent = cwd.as_path().join("absent.txt");
    std::fs::write(&tracked, "original\n").unwrap();
    std::fs::write(&untracked, "untracked\n").unwrap();
    for args in [vec!["init", "-q"], vec!["add", "--", "tracked.txt"]] {
        let output = std::process::Command::new("git")
            .current_dir(&cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let initial = create(&context).await;
    let observed = call(
        &context,
        json!({"operation":"observe","inventory_id":initial["inventory_id"],
        "category":"entrypoint","paths":[tracked, untracked, absent, tracked],"complete":true}),
    )
    .await
    .unwrap();
    assert_eq!(observed["summary"]["unique_candidates"], 3);
    let page = call(
        &context,
        json!({"operation":"read","inventory_id":observed["inventory_id"]}),
    )
    .await
    .unwrap();
    let records = page["records"].as_array().unwrap();
    let tracked_record = records
        .iter()
        .find(|record| record["candidate"]["id"].as_str() == tracked.to_str())
        .unwrap();
    let untracked_record = records
        .iter()
        .find(|record| record["candidate"]["id"].as_str() == untracked.to_str())
        .unwrap();
    let absent_record = records
        .iter()
        .find(|record| record["candidate"]["id"].as_str() == absent.to_str())
        .unwrap();
    assert_eq!(tracked_record["candidate"]["tracking"], "tracked");
    assert_eq!(untracked_record["candidate"]["tracking"], "untracked");
    assert_eq!(absent_record["candidate"]["exists"], false);
    assert_eq!(
        tracked_record["candidate"]["revision"],
        crate::tool_history::sha256(b"original\n")
    );
    // Classify and render actual observed identifiers all the way to the
    // user-deliverable artifact. An invented spelling cannot enter that set.
    let evidence = artifact(&context, json!({"checked": true})).await;
    let unsupported = call(
        &context,
        json!({
            "operation":"classify", "inventory_id":observed["inventory_id"],
            "decisions":[{"category":"entrypoint", "candidate_id":"invented.txt",
            "classification":"active", "evidence":[{"artifact_id":evidence}]}]
        }),
    )
    .await;
    assert!(unsupported.is_err());
    let classified = call(
        &context,
        json!({
            "operation":"classify", "inventory_id":observed["inventory_id"],
            "decisions":[{"category":"entrypoint", "candidate_id":tracked,
            "classification":"active", "evidence":[{"artifact_id":evidence}]}]
        }),
    )
    .await
    .unwrap();
    let rendered = call_through_registry(
        &context,
        json!({"operation":"render",
        "inventory_id":classified["inventory_id"], "classifications":["active"]}),
    )
    .await
    .unwrap();
    let final_artifact: Value = serde_json::from_slice(
        &std::fs::read(rendered["rendered_path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(final_artifact["identifiers"], json!([tracked]));
    assert_eq!(final_artifact["count"], 1);
    assert_eq!(rendered["count"], final_artifact["count"]);
    std::fs::write(&tracked, "different\n").unwrap();
    let refreshed = call(
        &context,
        json!({"operation":"observe","inventory_id":observed["inventory_id"],
        "category":"entrypoint","paths":[tracked,untracked,absent],"complete":true}),
    )
    .await
    .unwrap();
    assert_eq!(refreshed["changes"]["counts"]["changed"], 1);
    assert_eq!(refreshed["changes"]["counts"]["unchanged"], 2);
}

async fn call(context: &ToolInvocation, arguments: Value) -> Result<Value, FunctionCallError> {
    let payload = ToolPayload::Function {
        arguments: arguments.to_string(),
    };
    RetainedInventoryHandler
        .handle(ToolInvocation {
            session: Arc::clone(&context.session),
            step_context: Arc::clone(&context.step_context),
            cancellation_token: context.cancellation_token.clone(),
            tracker: Arc::clone(&context.tracker),
            call_id: context.call_id.clone(),
            tool_name: context.tool_name.clone(),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .map(|output| output.code_mode_result(&payload))
}

async fn call_through_registry(
    context: &ToolInvocation,
    arguments: Value,
) -> Result<Value, FunctionCallError> {
    let registry =
        crate::tools::registry::ToolRegistry::from_tools([
            Arc::new(RetainedInventoryHandler) as Arc<dyn CoreToolRuntime>
        ]);
    let dispatch = Arc::new(crate::tools::context::ToolDispatchState::new());
    assert!(dispatch.try_admit());
    registry
        .dispatch_any_with_terminal_outcome(
            ToolInvocation {
                session: Arc::clone(&context.session),
                step_context: Arc::clone(&context.step_context),
                cancellation_token: context.cancellation_token.clone(),
                tracker: Arc::clone(&context.tracker),
                call_id: "render-through-registry".to_string(),
                tool_name: ToolName::plain("inventory"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
            },
            dispatch,
        )
        .await
        .map(|output| output.code_mode_result())
}

async fn artifact(context: &ToolInvocation, value: Value) -> String {
    persist(context, value).await.unwrap()
}

async fn create(context: &ToolInvocation) -> Value {
    call(context, json!({"operation":"create","scope":{"roots":["src"],"exclusions":["generated"]},
        "profile":{"categories":["entrypoint","configuration"],"classifications":["active","dormant","excluded"],
        "required_categories":["entrypoint","configuration"]}})).await.unwrap()
}

fn candidate(id: &str, revision: &str) -> Value {
    json!({"id":id,"exists":true,"tracking":"tracked","revision":revision})
}

async fn import(
    context: &ToolInvocation,
    inventory: &Value,
    category: &str,
    candidates: Vec<Value>,
    complete: bool,
) -> Value {
    let source = artifact(context, json!({"scope_id":inventory["summary"]["scope_id"],"category":category,
        "complete":complete,"unresolved_reason":if complete {None} else {Some("query coverage incomplete")},
        "candidates":candidates})).await;
    call(context, json!({"operation":"import","inventory_id":inventory["inventory_id"],"source":{"artifact_id":source}})).await.unwrap()
}

#[tokio::test]
async fn inventory_lifecycle_retains_exact_ids_evidence_counts_and_scope() {
    let context = context().await;
    let initial = create(&context).await;
    assert_eq!(initial["summary"]["complete"], false);
    let imported = import(
        &context,
        &initial,
        "entrypoint",
        vec![
            candidate("src/a space~file.rs", "one"),
            candidate("src/b.rs", "two"),
            candidate("src/a space~file.rs", "one"),
        ],
        true,
    )
    .await;
    assert_eq!(imported["summary"]["unique_candidates"], 2);
    assert_eq!(imported["changes"]["counts"]["added"], 2);
    let covered = import(&context, &imported, "configuration", vec![], true).await;
    assert_eq!(
        covered["summary"]["unresolved_required_categories"],
        json!([])
    );
    assert_eq!(covered["summary"]["complete"], false);

    let text = create_canonical_output_artifact(
        &context.step_context.turn.config.codex_home,
        &context.session.thread_id.to_string(),
        &CanonicalToolResult::text("entrypoint evidence\nirrelevant body\n".to_string()),
    )
    .await;
    let evidence = json!({"artifact_id":text.artifact_id().unwrap(),"lines":[1,1]});
    let classified = call(&context, json!({"operation":"classify","inventory_id":covered["inventory_id"],
        "decisions":[{"category":"entrypoint","candidate_id":"src/a space~file.rs","classification":"active","evidence":[evidence]},
        {"category":"entrypoint","candidate_id":"src/b.rs","classification":"excluded","evidence":[evidence]}]})).await.unwrap();
    assert_eq!(classified["summary"]["complete"], true);
    let page = call(
        &context,
        json!({"operation":"read","inventory_id":classified["inventory_id"],"limit":1}),
    )
    .await
    .unwrap();
    assert_eq!(page["records"][0]["candidate"]["id"], "src/a space~file.rs");
    assert_eq!(
        page["records"][0]["record"]["pointer"],
        "/records/entrypoint/src~1a space~0file.rs"
    );
    assert_eq!(page["page_complete"], false);
    assert_eq!(page["next_offset"], 1);
    assert!(!page.to_string().contains("entrypoint evidence"));

    let reference: Source = serde_json::from_value(page["records"][0]["record"].clone()).unwrap();
    let (record, _) = read_source(&context, &reference).await.unwrap();
    assert_eq!(record["evidence"][0]["source"]["lines"], json!([1, 1]));
    assert_eq!(record["classification"], "active");
    assert_eq!(record["scope"], initial["summary"]["scope_id"]);
    assert_eq!(record["provenance"]["source"]["pointer"], "/candidates/0");
    let reference: Source = serde_json::from_value(record["provenance"]["source"].clone()).unwrap();
    assert_eq!(
        read_source(&context, &reference).await.unwrap().1.sha256,
        record["provenance"]["sha256"]
    );

    let rendered = call(
        &context,
        json!({"operation":"render","inventory_id":classified["inventory_id"],
        "classifications":["active"]}),
    )
    .await
    .unwrap();
    assert_eq!(rendered["count"], 1);
    let file: Value = serde_json::from_slice(
        &std::fs::read(rendered["rendered_path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(file["identifiers"], json!(["src/a space~file.rs"]));
    let (rendered, _) = read_source(
        &context,
        &Source {
            artifact_id: rendered["rendered_artifact_id"]
                .as_str()
                .unwrap()
                .to_string(),
            pointer: String::new(),
            lines: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(rendered["identifiers"], json!(["src/a space~file.rs"]));
    assert_eq!(rendered["count"], 1);
    assert_eq!(rendered["summary"]["complete"], true);
    // Original snapshots remain addressable; a later classification must not
    // overwrite the enumeration or manufacture classifications in its parent.
    let old = call(
        &context,
        json!({"operation":"read","inventory_id":imported["inventory_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(old["summary"]["statuses"]["unclassified"], 2);
}

#[tokio::test]
async fn inventory_refresh_reuses_unchanged_and_invalidates_only_changed_candidates() {
    let context = context().await;
    let initial = create(&context).await;
    let imported = import(
        &context,
        &initial,
        "entrypoint",
        vec![candidate("a", "one"), candidate("b", "two")],
        true,
    )
    .await;
    let same = import(
        &context,
        &imported,
        "entrypoint",
        vec![candidate("b", "two"), candidate("a", "one")],
        true,
    )
    .await;
    assert_eq!(same["inventory_id"], imported["inventory_id"]);
    assert_eq!(same["reused"], true);
    assert_eq!(
        same["changes"]["counts"],
        json!({"added":0,"changed":0,"removed":0,"unverified":0,"unchanged":2})
    );
    let evidence = artifact(&context, json!({"observation":"evidence"})).await;
    let classified=call(&context,json!({"operation":"classify","inventory_id":same["inventory_id"],
        "decisions":[{"category":"entrypoint","candidate_id":"a","classification":"active","evidence":[{"artifact_id":evidence}]},
        {"category":"entrypoint","candidate_id":"b","classification":"active","evidence":[{"artifact_id":evidence}]}]})).await.unwrap();
    let refreshed = import(
        &context,
        &classified,
        "entrypoint",
        vec![candidate("a", "changed"), candidate("b", "two")],
        true,
    )
    .await;
    assert_eq!(refreshed["changes"]["counts"]["changed"], 1);
    assert_eq!(refreshed["summary"]["statuses"]["classified"], 1);
    assert_eq!(refreshed["summary"]["statuses"]["stale"], 1);
    let page = call(
        &context,
        json!({"operation":"read","inventory_id":refreshed["inventory_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(page["records"][0]["status"], "stale");
    assert_eq!(page["records"][1]["status"], "classified");
    let partial = import(&context, &refreshed, "entrypoint", vec![], false).await;
    assert_eq!(partial["summary"]["unique_candidates"], 2);
    assert_eq!(partial["changes"]["counts"]["removed"], 0);
    let removed = import(
        &context,
        &partial,
        "entrypoint",
        vec![candidate("b", "two")],
        true,
    )
    .await;
    assert_eq!(removed["changes"]["counts"]["removed"], 1);
    let changes: Source = Source {
        artifact_id: removed["changes"]["artifact_id"]
            .as_str()
            .unwrap()
            .to_string(),
        pointer: "/removed".to_string(),
        lines: None,
    };
    assert_eq!(
        read_source(&context, &changes).await.unwrap().0,
        json!(["a"])
    );
}

#[tokio::test]
async fn inventory_fresh_unknown_revisions_require_review() {
    let context = context().await;
    let initial = create(&context).await;
    let unknown = json!({"id":"large-file","exists":true,"tracking":"unknown","revision":null});
    let imported = import(
        &context,
        &initial,
        "entrypoint",
        vec![unknown.clone()],
        true,
    )
    .await;
    let evidence = artifact(&context, json!({"observed":"active"})).await;
    let classified = call(
        &context,
        json!({"operation":"classify","inventory_id":imported["inventory_id"],
        "decisions":[{"category":"entrypoint","candidate_id":"large-file","classification":"active",
        "evidence":[{"artifact_id":evidence}]}]}),
    )
    .await
    .unwrap();
    let refreshed = import(&context, &classified, "entrypoint", vec![unknown], true).await;
    assert_eq!(refreshed["changes"]["counts"]["unverified"], 1);
    assert_eq!(refreshed["changes"]["counts"]["changed"], 0);
    assert_eq!(refreshed["summary"]["statuses"]["stale"], 1);
    let page = call(
        &context,
        json!({"operation":"read","inventory_id":refreshed["inventory_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(page["records"][0]["classification"], "active");
    assert!(
        page["records"][0]["unresolved_reason"]
            .as_str()
            .unwrap()
            .contains("unknown")
    );
    let rendered = call(
        &context,
        json!({"operation":"render","inventory_id":refreshed["inventory_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(rendered["count"], 0);
    assert_eq!(rendered["summary"]["complete"], false);
}

#[tokio::test]
async fn inventory_render_deduplicates_categories_without_hiding_incomplete_coverage() {
    let context = context().await;
    let initial = create(&context).await;
    let first = import(
        &context,
        &initial,
        "entrypoint",
        vec![candidate("shared", "one")],
        true,
    )
    .await;
    let second = import(
        &context,
        &first,
        "configuration",
        vec![candidate("shared", "one")],
        false,
    )
    .await;
    let evidence = artifact(&context, json!({"observed":"active"})).await;
    let decisions: Vec<Value> = ["entrypoint","configuration"].into_iter().map(|category|
        json!({"category":category,"candidate_id":"shared","classification":"active","evidence":[{"artifact_id":evidence}]})).collect();
    let classified = call(
        &context,
        json!({"operation":"classify","inventory_id":second["inventory_id"],"decisions":decisions}),
    )
    .await
    .unwrap();
    let rendered = call(
        &context,
        json!({"operation":"render","inventory_id":classified["inventory_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(rendered["count"], 1);
    assert_eq!(rendered["summary"]["complete"], false);
    assert_eq!(
        rendered["summary"]["unresolved_required_categories"],
        json!(["configuration"])
    );
    let file: Value = serde_json::from_slice(
        &std::fs::read(rendered["rendered_path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(file["identifiers"], json!(["shared"]));
}

#[tokio::test]
async fn inventory_evidence_accepts_exact_small_ranges_and_rejects_truncation_or_missing_sources() {
    let context = context().await;
    let initial = create(&context).await;
    let imported = import(
        &context,
        &initial,
        "entrypoint",
        vec![candidate("a", "one")],
        true,
    )
    .await;
    let output = create_canonical_output_artifact(
        &context.step_context.turn.config.codex_home,
        &context.session.thread_id.to_string(),
        &CanonicalToolResult::text(format!(
            "exact evidence\n{}",
            "x".repeat(MAX_SNAPSHOT_BYTES)
        )),
    )
    .await;
    let artifact_id = output.artifact_id().unwrap();
    let decision = |source: Value| {
        json!({"operation":"classify","inventory_id":imported["inventory_id"],
        "decisions":[{"category":"entrypoint","candidate_id":"a","classification":"active","evidence":[source]}]})
    };
    let classified = call(
        &context,
        decision(json!({"artifact_id":artifact_id,"lines":[1,1]})),
    )
    .await
    .unwrap();
    assert_eq!(classified["summary"]["statuses"]["classified"], 1);
    for source in [
        json!({"artifact_id":artifact_id,"lines":[1,2]}),
        json!({"artifact_id":artifact_id,"lines":[0,1]}),
        json!({"artifact_id":artifact_id,"pointer":"/bad~escape"}),
        json!({"artifact_id":"00000000-0000-4000-8000-000000000001","lines":[1,1]}),
    ] {
        assert!(call(&context, decision(source)).await.is_err());
    }
    let old = call(
        &context,
        json!({"operation":"read","inventory_id":imported["inventory_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(old["summary"]["statuses"]["unclassified"], 1);
}

#[tokio::test]
async fn inventory_rejects_mixed_scope_conflicts_missing_evidence_and_cancellation() {
    let context = context().await;
    let initial = create(&context).await;
    for source in [
        json!({"scope_id":"different","category":"entrypoint","complete":true,"candidates":[]}),
        json!({"scope_id":initial["summary"]["scope_id"],"category":"other","complete":true,"candidates":[]}),
        json!({"scope_id":initial["summary"]["scope_id"],"category":"entrypoint","complete":true,"candidates":[candidate("a","one"),candidate("a","two")]}),
        json!({"scope_id":initial["summary"]["scope_id"],"category":"entrypoint","complete":false,"candidates":[]}),
    ] {
        let source = artifact(&context, source).await;
        assert!(call(&context,json!({"operation":"import","inventory_id":initial["inventory_id"],"source":{"artifact_id":source}})).await.is_err());
    }
    let populated = import(
        &context,
        &initial,
        "entrypoint",
        vec![candidate("a", "one")],
        true,
    )
    .await;
    let evidence = artifact(&context, json!({"present":true})).await;
    for decision in [
        json!({"category":"entrypoint","candidate_id":"invented","classification":"active","evidence":[{"artifact_id":evidence}]}),
        json!({"category":"entrypoint","candidate_id":"a","classification":"undeclared","evidence":[{"artifact_id":evidence}]}),
        json!({"category":"entrypoint","candidate_id":"a","classification":"active","evidence":[]}),
        json!({"category":"entrypoint","candidate_id":"a","classification":"active","evidence":[{"artifact_id":evidence,"pointer":"/absent"}]}),
    ] {
        assert!(call(&context,json!({"operation":"classify","inventory_id":populated["inventory_id"],"decisions":[decision]})).await.is_err());
    }
    let unchanged = call(
        &context,
        json!({"operation":"read","inventory_id":populated["inventory_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(unchanged["summary"]["statuses"]["unclassified"], 1);
    context.cancellation_token.cancel();
    assert!(
        call(
            &context,
            json!({"operation":"read","inventory_id":populated["inventory_id"]})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn inventory_pages_are_bounded_and_exact_output_uses_existing_recovery() {
    let context = context().await;
    let initial = create(&context).await;
    let candidates = (0..120)
        .map(|i| candidate(&format!("src/{i:04}-{}.rs", "x".repeat(60)), "unchanged"))
        .collect();
    let imported = import(&context, &initial, "entrypoint", candidates, true).await;
    let mut offset = 0usize;
    let mut ids = BTreeSet::new();
    loop {
        let page=call(&context,json!({"operation":"read","inventory_id":imported["inventory_id"],"offset":offset,"limit":50})).await.unwrap();
        assert!(codex_utils_string::approx_token_count(&page.to_string()) < 3_000);
        for record in page["records"].as_array().unwrap() {
            assert!(ids.insert(record["candidate"]["id"].as_str().unwrap().to_string()));
        }
        let Some(next) = page["next_offset"].as_u64() else {
            assert_eq!(page["page_complete"], true);
            break;
        };
        assert!(next as usize > offset);
        offset = next as usize;
    }
    assert_eq!(ids.len(), 120);
    let (recovered, _) = read_tool_output_selectors_with_reuse(
        &context.step_context.turn.config.codex_home,
        &context.session.thread_id.to_string(),
        imported["inventory_id"].as_str().unwrap(),
        vec![ToolOutputSelector::JsonPointer {
            pointer: "/scope".to_string(),
        }],
    )
    .await
    .unwrap();
    assert!(recovered.complete);
    assert_eq!(recovered.results.len(), 1);
}

#[tokio::test]
async fn inventory_artifacts_are_confined_to_the_owning_task() {
    let owner = context().await;
    let inventory = create(&owner).await;
    let mut outsider = context().await;
    // Use the same storage root and a distinct session id, as actual sibling
    // tasks do, rather than merely relying on separate temporary directories.
    outsider.step_context = Arc::clone(&owner.step_context);
    assert!(
        call(
            &outsider,
            json!({"operation":"read","inventory_id":inventory["inventory_id"]})
        )
        .await
        .is_err()
    );
}

#[test]
fn inventory_schema_accepts_operations_and_rejects_unknown_fields() {
    let ToolSpec::Function(spec) = RetainedInventoryHandler.spec() else {
        panic!("function spec")
    };
    let schema = serde_json::to_value(spec.parameters).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&json!({"operation":"read","inventory_id":"id","limit":20})));
    assert!(validator.is_valid(&json!({"operation":"classify","inventory_id":"id","decisions":[
        {"category":"a","candidate_id":"x","classification":"active","evidence":[{"artifact_id":"id","lines":[2,4]}]}
    ]})));
    assert!(
        !validator.is_valid(&json!({"operation":"read","inventory_id":"id","scope":"different"}))
    );
    assert!(!validator.is_valid(&json!({"operation":"read","inventory_id":"id","limit":0})));
}
