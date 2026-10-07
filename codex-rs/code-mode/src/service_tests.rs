use std::sync::Arc;
use std::time::Duration;

use super::CellId;
use super::FallbackCodeModeSessionProvider;
use super::InProcessCodeModeSession;
use super::InProcessCodeModeSessionProvider;
use super::RuntimeResponse;
use super::WaitOutcome;
use super::WaitRequest;
use super::runtime_request;
use crate::CodeModeNestedToolCall;
use crate::CodeModeSessionDelegate;
use crate::CodeModeSessionProvider;
use crate::CodeModeToolKind;
use crate::ExecuteRequest;
use crate::FunctionCallOutputContentItem;
use crate::NestedCancellation;
use crate::NoopCodeModeSessionDelegate;
use crate::NotificationFuture;
use crate::ProcessOwnedCodeModeSessionProvider;
use crate::ToolDefinition;
use crate::ToolInvocationFuture;
use crate::runtime::MAX_SESSION_STORED_VALUE_BYTES;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

/// Answers every nested tool call with its own name and input so tests can
/// prove which tool a call form reached.
struct EchoDelegate;

struct OutputBudgetDelegate;

#[tokio::test]
async fn first_wait_restores_receipt_without_starting_a_cell() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let service = InProcessCodeModeSession::new();
    let mut request = execute_request(r#"store("kept", 42); text("original receipt");"#);
    request.state_path = Some(path.clone());
    request.yield_time_ms = Some(60_000);
    let started = service.execute(request).await.unwrap();
    let id = started.cell_id.clone();
    let original = started.initial_response().await.unwrap();
    service.shutdown().await.unwrap();
    drop(service);
    let restored = InProcessCodeModeSession::new();
    let recovery = Some(crate::ReceiptRecovery { path, terminal_only: false });
    assert_eq!(restored.wait(WaitRequest {
        cell_id: id, yield_time_ms: 1, recovery,
    }).await.unwrap(), WaitOutcome::LiveCell(original));
    restored.shutdown().await.unwrap();
}

#[tokio::test]
async fn terminated_receipt_recovers_without_cancelled_values_or_replayed_effects() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let delegate = Arc::new(ReceiptEffectDelegate {
        calls: std::sync::atomic::AtomicUsize::new(0),
        settled: tokio::sync::Notify::new(),
    });
    let service = InProcessCodeModeSession::with_delegate(delegate.clone());
    let mut request = execute_request(r#"
store("cancelled", true);
const receipt = await tools.exec_command({sentinel: "settled"});
text(receipt);
await notify("settled");
await new Promise(() => {});
"#);
    request.enabled_tools = vec![exec_command_definition()].into();
    request.state_path = Some(path.clone());
    request.yield_time_ms = Some(60_000);
    let started = service.execute(request).await.unwrap();
    let id = started.cell_id.clone();
    // Wait for the nested invocation and buffered evidence, without observing
    // (and consuming) that evidence before cancellation.
    tokio::time::timeout(Duration::from_secs(5), delegate.settled.notified()).await.unwrap();
    let terminal = service.terminate(id.clone()).await.unwrap();
    let WaitOutcome::LiveCell(RuntimeResponse::Terminated { ref content_items, .. }) = terminal else {
        panic!("expected termination");
    };
    assert!(content_items.iter().any(|item| matches!(item,
        FunctionCallOutputContentItem::InputText { text } if text.contains("settled"))));
    drop(started);
    service.shutdown().await.unwrap();
    drop(service);
    let restored = InProcessCodeModeSession::new();
    assert_eq!(restored.wait(WaitRequest {
        cell_id: id, yield_time_ms: 1,
        recovery: Some(crate::ReceiptRecovery { path: path.clone(), terminal_only: false }),
    }).await.unwrap(), terminal);
    let snapshot: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(snapshot["values"].get("cancelled").is_none());
    assert_eq!(delegate.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    restored.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_receipt_lookup_cannot_alias_a_new_nondurable_cell() {
    let directory = tempfile::tempdir().unwrap();
    let service = InProcessCodeModeSession::new();
    let started = service.execute(execute_request("text('new cell');")).await.unwrap();
    let id = started.cell_id.clone();
    started.initial_response().await.unwrap();
    let outcome = service.wait(WaitRequest {
        cell_id: id, yield_time_ms: 1,
        recovery: Some(crate::ReceiptRecovery { path: directory.path().join("absent.json"), terminal_only: true }),
    }).await.unwrap();
    assert!(matches!(outcome, WaitOutcome::MissingCell(_)));
    service.shutdown().await.unwrap();
}

struct ReceiptEffectDelegate {
    calls: std::sync::atomic::AtomicUsize,
    settled: tokio::sync::Notify,
}

impl CodeModeSessionDelegate for ReceiptEffectDelegate {
    fn invoke_tool<'a>(&'a self, _: CodeModeNestedToolCall, _: NestedCancellation) -> ToolInvocationFuture<'a> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(serde_json::json!({"receipt": "settled"})) })
    }
    fn notify<'a>(&'a self, _: String, _: CellId, _: String, _: CancellationToken) -> NotificationFuture<'a> {
        self.settled.notify_one();
        Box::pin(async { Ok(()) })
    }
    fn cell_closed(&self, _: &CellId) {}
}

impl CodeModeSessionDelegate for OutputBudgetDelegate {
    fn invoke_tool<'a>(&'a self, invocation: CodeModeNestedToolCall, _cancel: NestedCancellation) -> ToolInvocationFuture<'a> {
        Box::pin(async move { Ok(serde_json::json!(invocation.buffered_output_bytes)) })
    }

    fn notify<'a>(&'a self, _call_id: String, _cell_id: CellId, _text: String, _cancel: CancellationToken) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

#[tokio::test]
async fn nested_budget_counts_printed_output_not_unprinted_results() {
    let service = InProcessCodeModeSession::with_delegate(Arc::new(OutputBudgetDelegate));
    let response = execute(&service, ExecuteRequest {
        enabled_tools: vec![exec_command_definition()].into(),
        yield_time_ms: None,
        ..execute_request(r#"
const first = await tools.exec_command({});
const second = await tools.exec_command({});
if (first !== 0 || second !== 0) throw Error('unprinted results consumed budget');
text('x'.repeat(3000));
const used = await tools.exec_command({});
if (used < 3000) throw Error('printed output not counted');
text('budget accounting passed');
"#)
    }).await;
    assert!(result_text(&response).ends_with("budget accounting passed"));
}

impl CodeModeSessionDelegate for EchoDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        _cancellation_token: NestedCancellation,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            Ok(serde_json::json!({
                "tool": invocation.tool_name.to_string(),
                "input": invocation.input,
            }))
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

fn exec_command_definition() -> ToolDefinition {
    ToolDefinition {
        name: "exec_command".to_string(),
        tool_name: ToolName::plain("exec_command"),
        description: "run a command".into(),
        kind: CodeModeToolKind::Function,
        input_schema: None,
        default_timeout_ms: None,
        output_schema: None,
    }
}

fn result_text(response: &RuntimeResponse) -> String {
    let RuntimeResponse::Result {
        content_items,
        error_text: None,
        ..
    } = response
    else {
        panic!("expected a completed cell, got {response:?}");
    };
    content_items
        .iter()
        .map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => text.clone(),
            other => panic!("unexpected content item {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn execute_request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        state_path: None,
        tool_call_id: "call_1".to_string(),
        enabled_tools: Vec::new().into(),
        source: source.to_string(),
        yield_time_ms: Some(1),
        max_output_tokens: None,
        default_tool_timeout_ms: None,
    }
}

fn cell_id(value: &str) -> CellId {
    CellId::new(value.to_string())
}

#[tokio::test]
async fn fallback_provider_uses_in_process_session_when_host_is_missing() {
    let provider = FallbackCodeModeSessionProvider::new(
        Arc::new(ProcessOwnedCodeModeSessionProvider::with_host_program(
            "codex-code-mode-host-does-not-exist".into(),
        )),
        Arc::new(InProcessCodeModeSessionProvider),
    );

    let session = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .expect("missing process host should fall back to an in-process session");
    let response = session
        .execute(execute_request("text('fallback-ready');"))
        .await
        .expect("fallback execution should start")
        .initial_response()
        .await
        .expect("fallback execution should finish");

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "fallback-ready".to_string(),
            }],
            error_text: None,
        }
    );
}

#[test]
fn yield_time_does_not_extend_the_default_nested_tool_timeout() {
    let request = runtime_request(ExecuteRequest {
        yield_time_ms: Some(120_000),
        ..execute_request("text('done');")
    });

    assert_eq!(request.default_tool_timeout_ms, 60_000);
}

#[test]
fn host_supplied_default_nested_tool_timeout_reaches_the_runtime() {
    let request = runtime_request(ExecuteRequest {
        default_tool_timeout_ms: Some(75_000),
        ..execute_request("text('done');")
    });

    assert_eq!(request.default_tool_timeout_ms, 75_000);
}

#[test]
fn host_supplied_default_nested_tool_timeout_saturates_at_the_cap() {
    let request = runtime_request(ExecuteRequest {
        default_tool_timeout_ms: Some(codex_code_mode_protocol::MAX_TOOL_TIMEOUT_MS + 1),
        ..execute_request("text('done');")
    });

    assert_eq!(
        request.default_tool_timeout_ms,
        codex_code_mode_protocol::MAX_TOOL_TIMEOUT_MS
    );
}

async fn execute(service: &InProcessCodeModeSession, request: ExecuteRequest) -> RuntimeResponse {
    service
        .execute(request)
        .await
        .unwrap()
        .initial_response()
        .await
        .unwrap()
}

#[tokio::test]
async fn synchronous_exit_returns_successfully() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"text("before"); exit(); text("after");"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "before".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn timer_exit_returns_successfully() {
    let service = InProcessCodeModeSession::new();
    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
await new Promise(() => setTimeout(() => {
    text("before");
    store("timer-exit", "saved");
    exit();
    text("after");
}, 1));
text("after await");
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "before".to_string(),
            }],
            error_text: None,
        }
    );
    let stored = execute(
        &service,
        ExecuteRequest {
            source: r#"text(load("timer-exit"));"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;
    assert_eq!(result_text(&stored), "saved");
}

#[tokio::test]
async fn timer_throwing_exit_sentinel_remains_an_error() {
    let service = InProcessCodeModeSession::new();
    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"await new Promise(() => setTimeout(() => { throw "__codex_code_mode_exit__"; }, 1));"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: Vec::new(),
            error_text: Some("__codex_code_mode_exit__".to_string()),
        }
    );
}

#[tokio::test]
async fn oversized_cell_errors_are_bounded_without_losing_the_failure() {
    let service = InProcessCodeModeSession::new();
    for source in [
        "throw 'failure: ' + '🦀'.repeat(20000);",
        "throw {stack: 'failure: ' + '🦀'.repeat(20000)};",
        "await new Promise(() => setTimeout(() => { throw 'failure: ' + '🦀'.repeat(20000); }, 1));",
    ] {
        let response = execute(
            &service,
            ExecuteRequest {
                yield_time_ms: None,
                ..execute_request(source)
            },
        )
        .await;
        let RuntimeResponse::Result {
            error_text: Some(error),
            content_items,
            ..
        } = response
        else {
            panic!("expected a failed cell, got {response:?}");
        };
        assert!(content_items.is_empty());
        assert!(error.starts_with("failure: 🦀"));
        assert!(error.ends_with("\n[code mode error truncated]"));
        assert!(error.len() <= 16 * 1024);
    }

    let response = execute(
        &service,
        ExecuteRequest {
            yield_time_ms: None,
            ..execute_request("throw 'short error';")
        },
    )
    .await;
    assert!(matches!(response, RuntimeResponse::Result {
        error_text: Some(error), ..
    } if error == "short error"));
}

#[tokio::test]
async fn pending_timer_limit_rejects_overflow_and_releases_cleared_and_fired_slots() {
    let service = InProcessCodeModeSession::new();
    let response = execute(
        &service,
        ExecuteRequest {
            yield_time_ms: None,
            ..execute_request(
                r#"
const timers = Array.from({length: 128}, () => setTimeout(() => text("unexpected"), 60000));
try {
    setTimeout(() => text("overflow callback"), 1);
    text("overflow accepted");
} catch (error) {
    text(String(error));
}
clearTimeout(timers.pop());
await new Promise(resolve => setTimeout(resolve, 1));
await new Promise(resolve => setTimeout(resolve, 1));
for (const timer of timers) clearTimeout(timer);
text("slots released");
"#,
            )
        },
    )
    .await;
    assert_eq!(
        result_text(&response),
        "TypeError: code mode cell exceeded its limit of 128 pending timers\nslots released"
    );
}

#[tokio::test]
async fn compact_tool_discovery_resolves_one_exact_description() {
    let service = InProcessCodeModeSession::new();
    let response = execute(
        &service,
        ExecuteRequest {
            enabled_tools: vec![ToolDefinition {
                name: "sample-tool".to_string(),
                tool_name: ToolName::plain("sample-tool"),
                description: "exact schema description".into(),
                kind: CodeModeToolKind::Function,
                input_schema: None,
                default_timeout_ms: None,
                output_schema: None,
            }].into(),
            source: r#"
const all = ALL_TOOLS;
if (all !== ALL_TOOLS) throw new Error("discovery array must be memoized");
if (JSON.stringify(all) !== JSON.stringify([resolve_tool("sample_tool")])) throw new Error("discovery metadata differs");
all[0].description = "changed discovery copy";
text(JSON.stringify({ names: ALL_TOOL_NAMES, resolved: resolve_tool("sample_tool"), missing: resolve_tool("missing") === undefined }));
"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: r#"{"names":["sample_tool"],"resolved":{"name":"sample_tool","description":"exact schema description"},"missing":true}"#.to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn tool_callables_reuse_cell_catalog_but_not_changed_capabilities() {
    let service = InProcessCodeModeSession::new();
    for description in ["first schema", "changed schema"] {
        let response = execute(&service, ExecuteRequest {
            enabled_tools: vec![ToolDefinition {
                description: description.into(),
                ..exec_command_definition()
            }].into(),
            source: r#"
const resolved = resolve_tool("exec_command");
if (resolved !== tools.exec_command || resolved !== exec || resolved !== shell)
    throw new Error("callable was reconstructed");
tools.exec_command = null;
if (resolve_tool("exec_command") !== resolved) throw new Error("resolver trusted mutable tools");
text(resolved.description);
"#.into(),
            ..execute_request("")
        }).await;
        assert_eq!(result_text(&response), description);
    }
    let response = execute(&service, execute_request(
        "text(resolve_tool('exec_command') === undefined)"
    )).await;
    assert_eq!(result_text(&response), "true");
}

#[tokio::test]
async fn discovered_tools_are_callable_and_namespace_aliases_keep_exact_dispatch() {
    let service = InProcessCodeModeSession::with_delegate(Arc::new(EchoDelegate));
    let response = execute(
        &service,
        ExecuteRequest {
            enabled_tools: vec![ToolDefinition {
                name: "web__run".to_string(),
                tool_name: ToolName::plain("web__run"),
                description: "web schema".into(),
                ..exec_command_definition()
            }].into(),
            source: r#"
const web = await resolve_tool("web__run");
text({name: web.name, description: web.description, metadata: JSON.parse(JSON.stringify(web))});
text(await web({query: "resolved"}));
text(await tools.web.run({query: "namespace"}));
text(await tools.web__run({query: "canonical"}));
text({same: tools.web.run === tools.web__run,
      missing: resolve_tool("web__missing") === undefined,
      notEnabled: tools.web.missing === undefined,
      names: ALL_TOOL_NAMES});
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;
    let values: Vec<serde_json::Value> = result_text(&response)
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON result"))
        .collect();
    assert_eq!(
        values,
        vec![
            serde_json::json!({"name":"web__run", "description":"web schema", "metadata":{"name":"web__run", "description":"web schema"}}),
            serde_json::json!({"tool":"web__run", "input":{"query":"resolved"}}),
            serde_json::json!({"tool":"web__run", "input":{"query":"namespace"}}),
            serde_json::json!({"tool":"web__run", "input":{"query":"canonical"}}),
            serde_json::json!({"same":true, "missing":true, "notEnabled":true, "names":["web__run"]}),
        ]
    );
}

#[tokio::test]
async fn namespace_aliases_do_not_shadow_tools_or_inherit_prototype_members() {
    let service = InProcessCodeModeSession::with_delegate(Arc::new(EchoDelegate));
    let response = execute(
        &service,
        ExecuteRequest {
            enabled_tools: ["web__run", "web", "other__run", "__proto____run"]
                .into_iter()
                .map(|name| ToolDefinition {
                    name: name.to_string(),
                    tool_name: ToolName::plain(name),
                    ..exec_command_definition()
                })
                .collect(),
            source: r#"
text(await tools.web({query: "canonical namespace collision"}));
text(await tools.web__run({query: "flat survives"}));
text({noAlias: tools.web.run === undefined,
      noInheritedMember: tools.other.toString === undefined,
      prototypeUnchanged: Object.getPrototypeOf(tools) === Object.prototype,
      noPollution: Object.prototype.run === undefined});
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;
    let values: Vec<serde_json::Value> = result_text(&response)
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON result"))
        .collect();
    assert_eq!(
        values,
        vec![
            serde_json::json!({"tool":"web", "input":{"query":"canonical namespace collision"}}),
            serde_json::json!({"tool":"web__run", "input":{"query":"flat survives"}}),
            serde_json::json!({"noAlias":true, "noInheritedMember":true, "prototypeUnchanged":true, "noPollution":true}),
        ]
    );
}

#[tokio::test]
async fn resolve_tool_accepts_the_namespace_identity_reported_by_tool_search() {
    let service = InProcessCodeModeSession::with_delegate(Arc::new(EchoDelegate));
    let namespaced = |name: &str, namespace: &str, member: &str| ToolDefinition {
        name: name.to_string(),
        tool_name: ToolName::namespaced(namespace, member),
        description: format!("{member} schema").into(),
        ..exec_command_definition()
    };
    let response = execute(
        &service,
        ExecuteRequest {
            enabled_tools: vec![
                namespaced("mcp__github_fetch", "mcp__github", "_fetch"),
                namespaced("mcp__github_fetch_file", "mcp__github", "_fetch_file"),
                // One identity registered twice must not dispatch arbitrarily.
                namespaced("mcp__dup__first", "mcp__dup", "run"),
                namespaced("mcp__dup__second", "mcp__dup", "run"),
            ].into(),
            source: r#"
const fetch = resolve_tool("mcp__github._fetch");
text({name: fetch.name, description: fetch.description});
text(await fetch({url: "namespaced"}));
text({canonical: resolve_tool("mcp__github_fetch_file").name,
      wrongNamespace: resolve_tool("mcp__other._fetch") === undefined,
      missingMember: resolve_tool("mcp__github._missing") === undefined,
      ambiguous: resolve_tool("mcp__dup.run") === undefined});
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;
    let values: Vec<serde_json::Value> = result_text(&response)
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON result"))
        .collect();
    assert_eq!(
        values,
        vec![
            serde_json::json!({"name":"mcp__github_fetch", "description":"_fetch schema"}),
            serde_json::json!({"tool":"mcp__github___fetch", "input":{"url":"namespaced"}}),
            serde_json::json!({"canonical":"mcp__github_fetch_file", "wrongNamespace":true, "missingMember":true, "ambiguous":true}),
        ]
    );
}

#[tokio::test]
async fn stored_values_are_shared_between_cells_but_not_sessions() {
    let first_session = InProcessCodeModeSession::new();
    let second_session = InProcessCodeModeSession::new();

    let write_response = execute(
        &first_session,
        ExecuteRequest {
            source: r#"store("key", "visible");"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    let same_session = execute(
        &first_session,
        ExecuteRequest {
            source: r#"text(String(load("key")));"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;
    let other_session = execute(
        &second_session,
        ExecuteRequest {
            source: r#"text(String(load("key")));"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        write_response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: Vec::new(),
            error_text: None,
        }
    );
    assert_eq!(
        same_session,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("2"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "visible".to_string(),
            }],
            error_text: None,
        }
    );
    assert_eq!(
        other_session,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "undefined".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn oversized_store_rejects_all_writes_from_the_cell() {
    let session = InProcessCodeModeSession::new();
    let response = execute(
        &session,
        ExecuteRequest {
            source: format!(
                r#"store("partial", "must-not-commit"); store("oversized", "x".repeat({}));"#,
                MAX_SESSION_STORED_VALUE_BYTES + 1
            ),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    let RuntimeResponse::Result { error_text, .. } = response else {
        panic!("oversized store should complete with an error");
    };
    assert!(
        error_text
            .as_deref()
            .is_some_and(|error| error.contains("code mode session storage exceeds its limit"))
    );

    let read_response = execute(
        &session,
        ExecuteRequest {
            source: r#"text(String(load("partial")));"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;
    assert_eq!(
        read_response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("2"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "undefined".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn rejected_store_can_compact_settled_evidence_without_rerunning_producers() {
    let session = InProcessCodeModeSession::new();
    let response = execute(&session, ExecuteRequest {
        source: format!(r#"
            let producers = 0;
            const batch = await Promise.allSettled([Promise.resolve(++producers)]);
            store("prior", "preserved");
            let rejected = false;
            try {{ store("batch", {{ batch, padding: "x".repeat({}) }}); }}
            catch (_) {{ rejected = true; }}
            if (!rejected || producers !== 1) throw new Error("admission fixture failed");
            store("batch", batch);
            text("retained");
        "#, MAX_SESSION_STORED_VALUE_BYTES + 1),
        yield_time_ms: None,
        ..execute_request("")
    }).await;
    assert!(matches!(response, RuntimeResponse::Result { error_text: None, .. }), "{response:?}");
    let response = execute(&session, ExecuteRequest {
        source: r#"text([load("prior"), load("batch")]);"#.to_string(),
        yield_time_ms: None,
        ..execute_request("")
    }).await;
    let RuntimeResponse::Result { error_text: None, content_items, .. } = response else {
        panic!("compacted evidence must commit: {response:?}");
    };
    let [FunctionCallOutputContentItem::InputText { text }] = content_items.as_slice() else {
        panic!("expected retained evidence");
    };
    assert_eq!(serde_json::from_str::<serde_json::Value>(text).unwrap(),
        serde_json::json!(["preserved", [{"status":"fulfilled","value":1}]]));
}

#[tokio::test]
async fn store_replacement_releases_bytes_in_current_and_later_cells() {
    let session = InProcessCodeModeSession::new();
    let first = execute(&session, ExecuteRequest {
        source: r#"store("a", "x".repeat(4 * 1024 * 1024)); store("a", "small"); store("b", "y".repeat(6 * 1024 * 1024)); text(load("a"));"#.to_string(),
        yield_time_ms: None,
        ..execute_request("")
    }).await;
    assert_eq!(
        first,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "small".to_string()
            }],
            error_text: None,
        }
    );
    let second = execute(&session, ExecuteRequest {
        source: r#"text(load("b").length); store("b", "tiny"); store("c", "z".repeat(6 * 1024 * 1024)); text(load("b"));"#.to_string(),
        yield_time_ms: None,
        ..execute_request("")
    }).await;
    assert_eq!(
        second,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("2"),
            content_items: vec![
                FunctionCallOutputContentItem::InputText {
                    text: "6291456".to_string()
                },
                FunctionCallOutputContentItem::InputText {
                    text: "tiny".to_string()
                },
            ],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn shutdown_interrupts_cpu_bound_cells() {
    let service = InProcessCodeModeSession::new();

    let cell = service
        .execute(ExecuteRequest {
            source: "while (true) {}".to_string(),
            ..execute_request("")
        })
        .await
        .unwrap();
    assert_eq!(
        cell.initial_response().await.unwrap(),
        RuntimeResponse::Yielded {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
        }
    );

    tokio::time::timeout(Duration::from_secs(1), service.shutdown())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn start_cell_rejects_new_cell_after_shutdown_begins() {
    let service = InProcessCodeModeSession::new();
    service.shutdown().await.unwrap();

    let error = service
        .execute(execute_request("text('late');"))
        .await
        .err()
        .unwrap();

    assert_eq!(error, "code mode session is shutting down".to_string());
}

#[tokio::test]
async fn console_shim_forwards_to_text_instead_of_v8_console() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"console.log("alias", 1, { ok: true });
console.error("bad");
text(String(typeof console.info === "function"));"#
                .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![
                FunctionCallOutputContentItem::InputText {
                    text: r#"alias 1 {"ok":true}"#.to_string(),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "bad".to_string(),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "true".to_string(),
                },
            ],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn bare_exec_aliases_forward_to_the_exec_command_tool() {
    let service = InProcessCodeModeSession::with_delegate(Arc::new(EchoDelegate));

    let response = execute(
        &service,
        ExecuteRequest {
            enabled_tools: vec![exec_command_definition()].into(),
            source: r#"const viaExec = await exec({ cmd: "echo hi" });
const viaBare = await exec_command("echo again");
const viaShell = await shell({ cmd: "pwd" });
text(JSON.stringify([viaExec, viaBare, viaShell]));"#
                .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    let parsed: serde_json::Value =
        serde_json::from_str(&result_text(&response)).expect("alias results are JSON");
    assert_eq!(
        parsed,
        serde_json::json!([
            { "tool": "exec_command", "input": { "cmd": "echo hi" } },
            { "tool": "exec_command", "input": "echo again" },
            { "tool": "exec_command", "input": { "cmd": "pwd" } },
        ])
    );
}

#[tokio::test]
async fn exec_alias_without_exec_command_names_the_canonical_call_form() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"await exec("echo hi");"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    let RuntimeResponse::Result {
        error_text: Some(error_text),
        ..
    } = response
    else {
        panic!("expected a failed cell, got {response:?}");
    };
    assert!(error_text.contains("no `exec_command` nested tool is enabled"));
    assert!(error_text.contains("call `await tools.<name>(...)`"));
    assert!(error_text.contains("(no nested tools are enabled)"));
}

#[tokio::test]
async fn completion_budget_holds_a_cell_through_output_until_it_finishes() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"text("first");
await new Promise((resolve) => setTimeout(resolve, 200));
text("second");"#
                .to_string(),
            yield_time_ms: Some(10_000),
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![
                FunctionCallOutputContentItem::InputText {
                    text: "first".to_string(),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "second".to_string(),
                },
            ],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn date_locale_string_formats_with_icu_data() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
const value = new Date("2025-01-02T03:04:05Z")
  .toLocaleString("fr-FR", {
    weekday: "long",
    month: "long",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
    timeZone: "UTC",
  });
text(value);
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "jeudi 2 janvier \u{e0} 03:04:05".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn intl_date_time_format_formats_with_icu_data() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
const formatter = new Intl.DateTimeFormat("fr-FR", {
  weekday: "long",
  month: "long",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hour12: false,
  timeZone: "UTC",
});
text(formatter.format(new Date("2025-01-02T03:04:05Z")));
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "jeudi 2 janvier \u{e0} 03:04:05".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn bounded_parallel_notify_returns_delivery_promise() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
const notification = notify("ping");
const returnsExpectedTypes = [
  text("first") === undefined,
  image("data:image/png;base64,AAAA") === undefined,
  notification instanceof Promise,
];
await notification;
text(JSON.stringify(returnsExpectedTypes));
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![
                FunctionCallOutputContentItem::InputText {
                    text: "first".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAAA".to_string(),
                    detail: Some(crate::DEFAULT_IMAGE_DETAIL),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "[true,true,true]".to_string(),
                },
            ],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn image_helper_accepts_raw_mcp_image_block_with_original_detail() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image({
  type: "image",
  data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
  mimeType: "image/png",
  _meta: { "codex/imageDetail": "original" },
});
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

    assert_eq!(
            response,
            RuntimeResponse::Result {
                output_loss: None,
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string(),
                    detail: Some(crate::ImageDetail::Original),
                }],
                error_text: None,
            }
        );
}

#[tokio::test]
async fn generated_image_helper_appends_image_and_output_hint() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
generatedImage({
  image_url: "data:image/png;base64,AAAA",
  output_hint: "generated image save hint",
});
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAAA".to_string(),
                    detail: Some(crate::DEFAULT_IMAGE_DETAIL),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "generated image save hint".to_string(),
                },
            ],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn image_helper_second_arg_overrides_explicit_object_detail() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
image(
  {
    image_url: "data:image/png;base64,AAAA",
    detail: "high",
  },
  "original",
);
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAAA".to_string(),
                detail: Some(crate::ImageDetail::Original),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn image_helper_second_arg_overrides_raw_mcp_image_detail() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image(
  {
    type: "image",
    data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
    mimeType: "image/png",
    _meta: { "codex/imageDetail": "original" },
  },
  "high",
);
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

    assert_eq!(
            response,
            RuntimeResponse::Result {
                output_loss: None,
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string(),
                    detail: Some(crate::ImageDetail::High),
                }],
                error_text: None,
            }
        );
}

#[tokio::test]
async fn image_helper_accepts_low_detail() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
image({
  image_url: "data:image/png;base64,AAAA",
  detail: "low",
});
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAAA".to_string(),
                detail: Some(crate::ImageDetail::Low),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn image_helpers_reject_remote_urls() {
    for image_url in [
        "http://example.com/image.jpg",
        "https://example.com/image.jpg",
    ] {
        for source in [
            format!("image({image_url:?});"),
            format!("generatedImage({{ image_url: {image_url:?} }});"),
        ] {
            let service = InProcessCodeModeSession::new();

            let response = execute(
                &service,
                ExecuteRequest {
                    source,
                    yield_time_ms: None,
                    ..execute_request("")
                },
            )
            .await;

            assert_eq!(
                    response,
                    RuntimeResponse::Result {
                        output_loss: None,
                        cell_id: cell_id("1"),
                        content_items: Vec::new(),
                        error_text: Some(
                            "TypeError: Tool call failed: remote image URLs are not supported in tool outputs. Pass a base64 data URI instead\n    at exec_main.mjs:1:1".to_string(),
                        ),
                    }
                );
        }
    }
}

#[tokio::test]
async fn image_helper_rejects_unsupported_detail() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
image({
  image_url: "data:image/png;base64,AAAA",
  detail: "medium",
});
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell_id("1"),
            content_items: Vec::new(),
            error_text: Some("TypeError: image detail must be one of: auto, low, high, original\n    at exec_main.mjs:2:1".to_string()),
        }
    );
}

#[tokio::test]
async fn image_helper_rejects_raw_mcp_result_container() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image({
  content: [
    {
      type: "image",
      data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
      mimeType: "image/png",
      _meta: { "codex/imageDetail": "original" },
    },
  ],
  isError: false,
});
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

    assert_eq!(
            response,
            RuntimeResponse::Result {
                output_loss: None,
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                error_text: Some(
                    "TypeError: image expects a non-empty image URL string, an object with image_url and optional detail, or a raw MCP image block\n    at exec_main.mjs:2:1".to_string(),
                ),
            }
        );
}

#[tokio::test]
async fn wait_reports_missing_cell_separately_from_runtime_results() {
    let service = InProcessCodeModeSession::new();

    let response = service
        .wait(WaitRequest {
            recovery: None,
            cell_id: cell_id("missing"),
            yield_time_ms: 1,
        })
        .await
        .unwrap();

    let WaitOutcome::MissingCell(RuntimeResponse::Result { cell_id: id, content_items, error_text: Some(error), output_loss }) = response else {
        panic!("expected missing-cell receipt");
    };
    assert_eq!(id, cell_id("missing"));
    assert!(content_items.is_empty());
    assert!(output_loss.is_none());
    let receipt: serde_json::Value = serde_json::from_str(&error).unwrap();
    assert_eq!(receipt["status"], "unknown_cell");
    assert_eq!(receipt["terminal_state"], "unknown");
    assert_eq!(receipt["automatic_replay_allowed"], false);
}
