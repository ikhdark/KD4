use super::*;
use pretty_assertions::assert_eq;

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
    r = await tools.write_stdin({{session_id:r.session_id, chars:"", yield_time_ms:1000, max_output_tokens:1000}});
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
