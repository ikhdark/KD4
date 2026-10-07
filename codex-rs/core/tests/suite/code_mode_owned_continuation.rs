use super::*;
use pretty_assertions::assert_eq;

#[test_case::test_case(0; "success")]
#[test_case::test_case(7; "failure")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn awaited_command_finishes_before_predetermined_followup(exit_code: i32) -> Result<()> {
    require_network!();
    let server = responses::start_mock_server().await;
    let directory = tempfile::TempDir::new()?;
    let launches = directory.path().join("launches.txt");
    let python = which::which("python").or_else(|_| which::which("python3"))?;
    // Outlast the old 10-second nested default without asking the model or
    // JavaScript to poll. A failed command must not run the success followup.
    let command = serde_json::json!({
        "program": python,
        "args": ["-c", "import pathlib, sys, time; p = pathlib.Path(sys.argv[1]); p.write_bytes((p.read_bytes() if p.exists() else b'') + b'first\\n'); time.sleep(11); print('command-finished'); sys.exit(int(sys.argv[2]))", launches, exit_code.to_string()],
    });
    let followup = serde_json::json!({
        "program": python,
        "args": ["-c", "import pathlib, sys; p = pathlib.Path(sys.argv[1]); p.write_bytes(p.read_bytes() + b'followup\\n'); print('followup-finished')", launches],
    });
    let code = format!(
        r#"
const result = await tools.exec_command({command});
if (result.session_id || result.exit_code == null) throw Error("host handed off unfinished command");
if (result.exit_code === 0) {{
    const next = await tools.exec_command({followup});
    if (next.session_id || next.exit_code !== 0) throw Error("followup did not complete");
}}
text({{sequence_complete:true, exit_code:result.exit_code, output:result.output}});
"#
    );
    let (_test, completion) = run_code_mode_turn_with_model_and_config(
        &server,
        "Run the predetermined command sequence",
        &code,
        "gpt-6-astra",
        |_| {},
    )
    .await?;
    let request = completion.single_request();
    let (output, success) = custom_tool_output_body_and_success(&request, "call-1");
    assert_ne!(success, Some(false), "{output}");
    assert!(output.contains("\"sequence_complete\":true"), "{output}");
    assert!(
        output.contains(&format!("\"exit_code\":{exit_code}")),
        "{output}"
    );
    assert!(output.contains("command-finished"), "{output}");
    assert_eq!(
        fs::read_to_string(launches)?,
        if exit_code == 0 {
            "first\nfollowup\n"
        } else {
            "first\n"
        }
    );
    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path().contains("responses"))
            .count(),
        2,
        "only the sequence request and final answer request; no model polling"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn printed_process_receipts_do_not_interrupt_a_predetermined_drain() -> Result<()> {
    require_network!();
    let server = responses::start_mock_server().await;
    // Cross the former ten-second automatic yield with output already
    // buffered. The continuation is entirely owned by the JavaScript loop.
    let command = if cfg!(windows) {
        "[Console]::Out.Write('first-chunk'); Start-Sleep -Milliseconds 11000; [Console]::Out.Write('last-chunk')"
    } else {
        "printf first-chunk; sleep 11; printf last-chunk"
    };
    let code = format!(
        r#"
let r = await tools.exec_command({{cmd:{command:?}, yield_time_ms:1000, max_output_tokens:1000}});
text(r);
while (r.session_id && r.exit_code == null) {{
    r = await tools.write_stdin({{session_id:r.session_id,incarnation:r.session_capabilities.incarnation, chars:"", yield_time_ms:1000, max_output_tokens:1000}});
    text(r);
}}
text({{drain_complete:true, exit_code:r.exit_code}});
"#
    );
    let (_test, completion) =
        run_code_mode_turn(&server, "Run and drain the command", &code).await?;
    let request = completion.single_request();
    let (output, success) = custom_tool_output_body_and_success(&request, "call-1");
    assert_ne!(success, Some(false), "{output}");
    assert!(output.contains("\"drain_complete\":true"), "{output}");
    assert_eq!(output.matches("first-chunk").count(), 1, "{output}");
    assert_eq!(output.matches("last-chunk").count(), 1, "{output}");
    assert!(output.contains("\"exit_code\":0"), "{output}");
    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path().contains("responses"))
            .count(),
        2
    );
    Ok(())
}
