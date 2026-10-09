use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::InProcessCodeModeSession;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::V8JitMode;
use codex_code_mode::initialize_v8;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn code_mode_runs_with_jit_disabled() {
    initialize_v8(V8JitMode::Disabled).expect("initialize V8 without JIT");

    let service = InProcessCodeModeSession::new();
    let started = service
        .execute(ExecuteRequest {
            state_path: None,
            tool_call_id: "call_1".to_string(),
            enabled_tools: Vec::new().into(),
            source: "text(21 * 2);".to_string(),
            yield_time_ms: None,
            max_output_tokens: None,
            default_tool_timeout_ms: None,
        })
        .await
        .expect("start code-mode cell");
    let cell_id = started.cell_id.clone();
    let response = started
        .initial_response()
        .await
        .expect("execute code-mode cell");

    assert_eq!(
        response,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id,
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "42".to_string(),
            }],
            error_text: None,
        }
    );
    assert_eq!(
        initialize_v8(V8JitMode::Enabled),
        Err("V8 was already initialized with JIT disabled".to_string())
    );
    service.shutdown().await.expect("shutdown code-mode session");
}
