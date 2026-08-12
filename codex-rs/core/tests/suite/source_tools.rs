use codex_features::Feature;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::local;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;

fn call_output(req: &ResponsesRequest, call_id: &str) -> (String, Option<bool>) {
    let raw = req.function_call_output(call_id);
    assert_eq!(
        raw.get("call_id").and_then(Value::as_str),
        Some(call_id),
        "mismatched call_id in function_call_output"
    );
    let (content, success) = req
        .function_call_output_content_and_success(call_id)
        .expect("function_call_output present");
    (
        content.expect("function_call_output content present"),
        success,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_tools_execute_search_and_read_end_to_end() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::SourceTools)
            .expect("source tools feature should be enableable");
    });
    let test = builder.build(&server).await?;
    let cwd = test.config.cwd.clone();
    std::fs::create_dir_all(cwd.join(".git"))?;
    std::fs::create_dir_all(cwd.join("src"))?;
    std::fs::write(
        cwd.join("src/lib.rs"),
        "pub fn first() {}\npub fn readiness_needle() {}\npub fn third() {}\n",
    )?;

    let search_call_id = "source-search-call";
    let search_response = sse(vec![
        ev_response_created("resp-search"),
        ev_function_call(
            search_call_id,
            "search_source",
            &json!({"query": "readiness_needle"}).to_string(),
        ),
        ev_completed("resp-search"),
    ]);
    responses::mount_sse_once(&server, search_response).await;

    let read_call_id = "source-read-call";
    let read_response = sse(vec![
        ev_response_created("resp-read"),
        ev_function_call(
            read_call_id,
            "read_file_span",
            &json!({"path": "src/lib.rs", "start_line": 1, "line_count": 3}).to_string(),
        ),
        ev_completed("resp-read"),
    ]);
    let search_output_mock = responses::mount_sse_once(&server, read_response).await;

    let final_response = sse(vec![
        ev_response_created("resp-final"),
        ev_assistant_message("msg-final", "source tools complete"),
        ev_completed("resp-final"),
    ]);
    let read_output_mock = responses::mount_sse_once(&server, final_response).await;

    test.submit_turn_with_environments(
        "Search for readiness_needle, then read the matching file.",
        Some(vec![local(cwd)]),
    )
    .await?;

    let (search_output, search_success) =
        call_output(&search_output_mock.single_request(), search_call_id);
    assert_ne!(search_success, Some(false));
    assert!(
        search_output.contains("citation: src/lib.rs:2-2 (match line 2)"),
        "{search_output}"
    );
    assert!(search_output.contains("pub fn readiness_needle() {}"));

    let (read_output, read_success) = call_output(&read_output_mock.single_request(), read_call_id);
    assert_ne!(read_success, Some(false));
    assert!(
        read_output.contains("citation: src/lib.rs:1-3"),
        "{read_output}"
    );
    assert!(read_output.contains("     2 | pub fn readiness_needle() {}"));

    Ok(())
}
