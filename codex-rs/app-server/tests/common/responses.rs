use core_test_support::responses;
use serde_json::json;

pub fn create_apply_patch_sse_response(
    patch_content: &str,
    call_id: &str,
) -> anyhow::Result<String> {
    Ok(responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_apply_patch_custom_tool_call(call_id, patch_content),
        responses::ev_completed("resp-1"),
    ]))
}

pub fn create_exec_command_sse_response(call_id: &str) -> anyhow::Result<String> {
    let (cmd, args) = ("cmd.exe", vec!["/d", "/c", "echo hi"]);
    let command = std::iter::once(cmd.to_string())
        .chain(args.into_iter().map(str::to_string))
        .collect::<Vec<_>>();
    let tool_call_arguments = serde_json::to_string(&json!({
        "cmd": command.join(" "),
        "yield_time_ms": 500
    }))?;
    Ok(responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_function_call(call_id, "exec_command", &tool_call_arguments),
        responses::ev_completed("resp-1"),
    ]))
}

pub fn create_request_user_input_sse_response(call_id: &str) -> anyhow::Result<String> {
    let tool_call_arguments = serde_json::to_string(&json!({
        "questions": [{
            "id": "confirm_path",
            "header": "Confirm",
            "question": "Proceed with the plan?",
            "options": [{
                "label": "Yes (Recommended)",
                "description": "Continue the current plan."
            }, {
                "label": "No",
                "description": "Stop and revisit the approach."
            }]
        }]
    }))?;

    Ok(responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_function_call(call_id, "request_user_input", &tool_call_arguments),
        responses::ev_completed("resp-1"),
    ]))
}

pub fn create_request_permissions_sse_response(call_id: &str) -> anyhow::Result<String> {
    let tool_call_arguments = serde_json::to_string(&json!({
        "reason": "Select a workspace root",
        "permissions": {
            "file_system": {
                "write": [
                    ".",
                    "../shared"
                ]
            }
        }
    }))?;

    Ok(responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_function_call(call_id, "request_permissions", &tool_call_arguments),
        responses::ev_completed("resp-1"),
    ]))
}
