use super::*;

// Control files live outside the admitted workspace. Only this test control
// tool bypasses workspace admission, so it can release a deliberately held
// real child without weakening any production reader/writer contract.
struct ProcessGateControl(std::path::PathBuf);

impl ToolExecutor<ToolInvocation> for ProcessGateControl {
    fn tool_name(&self) -> codex_tools::ToolName {
        codex_tools::ToolName::plain("process_gate_control")
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        PacketTestTool { name: "process_gate_control" }.spec()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = invocation.payload else { panic!("function") };
            let args: serde_json::Value = serde_json::from_str(&arguments).unwrap();
            if args["release"] == true {
                tokio::fs::write(self.0.join("release"), b"go").await.unwrap();
            } else {
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !tokio::fs::try_exists(self.0.join("ready")).await.unwrap() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await.expect("real process must publish its ready barrier");
            }
            Ok(crate::tools::context::boxed_tool_output(
                FunctionToolOutput::from_text("ready".into(), Some(true)),
            ))
        })
    }
}

impl crate::tools::registry::CoreToolRuntime for ProcessGateControl {
    fn delegates_workspace_admission(&self) -> bool { true }
}

#[tokio::test]
async fn admission_grace_preserves_real_command_through_queued_read_and_command() {
    let workspace = tempfile::tempdir().unwrap();
    let control = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("read.txt"), "independent read\n").unwrap();
    let runtime = PacketRuntime::with_nested_runtimes_in_workspace(vec![
        Arc::new(crate::tools::handlers::ExecCommandHandler::default()),
        Arc::new(crate::tools::handlers::ReadFileHandler),
        Arc::new(ProcessGateControl(control.path().to_path_buf())),
    ], Some(workspace.path())).await;
    let script = "import pathlib,sys,time\nc=pathlib.Path(sys.argv[1])\n(c/'ready').write_text('ready')\nend=time.monotonic()+15\nwhile not (c/'release').exists():\n if time.monotonic()>end: raise RuntimeError('fixture release timed out')\n time.sleep(.01)\nprint('admitted-command-survived',flush=True)\n";
    let command = serde_json::json!({
        "program":"python", "args":["-u", "-c", script, control.path()],
        "yield_time_ms":10000, "stall_timeout_ms":0,
    });
    let queued_command = serde_json::json!({
        "program":"python", "args":["-c", "print('queued-command-ran')"],
        "yield_time_ms":1000,
    });
    let output = runtime.exec(&format!(r#"
        const running = tools.exec_command({command});
        await tools.process_gate_control({{}});
        const started = Date.now();
        // An uncaught Promise.all is intentional: admission failure must be
        // data, not a rejection that cancels the already-admitted process.
        const blocked = await Promise.all([
            tools.read_file({{path: "read.txt"}}, {{timeout_ms: 10000}}),
            tools.exec_command({queued_command}, {{timeout_ms: 10000}}),
        ]);
        for (const result of blocked) {{
            if (result.kind !== 'workspace_gate_timeout' || result.did_run !== false)
                throw new Error('queued operation must not execute');
            if (!result.output.includes('exec_command') || !result.output.includes('held for '))
                throw new Error('missing blocker attribution');
            text(result);
        }}
        if (Date.now() - started >= 5000) throw new Error('execution timeout became admission timeout');
        await tools.process_gate_control({{release: true}});
        const finished = await running;
        if (finished.exit_code !== 0 || finished.process_exited !== true)
            throw new Error('admitted process did not finish normally');
        text(finished);
        text(await tools.read_file({{path: "read.txt"}}));
    "#)).await;
    let visible = packet_output_text(output.as_ref());
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success, "{visible}");
    assert!(visible.contains("admitted-command-survived"), "{visible}");
    assert!(visible.contains("independent read"), "{visible}");
    assert!(!visible.contains("queued-command-ran"), "{visible}");
    assert!(runtime.session.list_background_terminals().await.is_empty());
    assert_eq!(runtime.step.turn.turn_timing_state.lifecycle_context().parallel_gate_waiter_count, 0);
    runtime.finish().await;
}
