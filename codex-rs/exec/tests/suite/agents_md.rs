#![allow(clippy::unwrap_used)]

use core_test_support::responses;
use core_test_support::test_codex_exec::test_codex_exec;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_includes_workspace_agents_md_in_request() -> anyhow::Result<()> {
    let test = test_codex_exec();
    std::fs::write(test.cwd_path().join("AGENTS.md"), "workspace instructions")?;

    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp1"),
        responses::ev_assistant_message("m1", "fixture hello"),
        responses::ev_completed("resp1"),
    ]);
    let response_mock = responses::mount_sse_once(&server, body).await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("tell me something")
        .assert()
        .success();

    let user_messages = response_mock.single_request().message_input_texts("user");
    assert!(
        user_messages
            .iter()
            .any(|text| text.contains("workspace instructions")),
        "request should include workspace AGENTS.md instructions: {user_messages:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_does_not_offer_request_user_input() -> anyhow::Result<()> {
    let test = test_codex_exec();

    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp1"),
        responses::ev_assistant_message("m1", "fixture hello"),
        responses::ev_completed("resp1"),
    ]);
    let response_mock = responses::mount_sse_once(&server, body).await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("tell me something")
        .assert()
        .success();

    // Exec rejects the client request, which interrupts the turn; the tool must not be callable.
    let mut tool_names = Vec::new();
    collect_named_objects(&response_mock.single_request().body_json(), &mut tool_names);
    assert!(
        tool_names.iter().any(|name| name == "exec"),
        "exec request should define its tools: {tool_names:?}"
    );
    assert!(
        !tool_names.iter().any(|name| name == "request_user_input"),
        "exec request should not define request_user_input: {tool_names:?}"
    );

    Ok(())
}

fn collect_named_objects(value: &serde_json::Value, names: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(name) = object.get("name").and_then(serde_json::Value::as_str) {
                names.push(name.to_string());
            }
            for child in object.values() {
                collect_named_objects(child, names);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_named_objects(item, names);
            }
        }
        _ => {}
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_prefers_workspace_agents_override_md() -> anyhow::Result<()> {
    let test = test_codex_exec();
    std::fs::write(test.cwd_path().join("AGENTS.md"), "base instructions")?;
    std::fs::write(
        test.cwd_path().join("AGENTS.override.md"),
        "override instructions",
    )?;

    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp1"),
        responses::ev_assistant_message("m1", "fixture hello"),
        responses::ev_completed("resp1"),
    ]);
    let response_mock = responses::mount_sse_once(&server, body).await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("tell me something")
        .assert()
        .success();

    let user_messages = response_mock.single_request().message_input_texts("user");
    assert!(
        user_messages
            .iter()
            .any(|text| text.contains("override instructions")),
        "request should include AGENTS.override.md instructions: {user_messages:?}"
    );
    assert!(
        user_messages
            .iter()
            .all(|text| !text.contains("base instructions")),
        "request should exclude shadowed AGENTS.md instructions: {user_messages:?}"
    );

    Ok(())
}
